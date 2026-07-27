use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use knot_runtime::{Clock, HttpTransport};
use knot_types::OfferedKey;
use russh::keys::ssh_key;
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId};
use tokio_util::task::TaskTracker;

use crate::SshState;
use crate::exec::run_exec;

pub(crate) struct KnotSshServer<H, C> {
    pub(crate) state: Arc<SshState<H, C>>,
    pub(crate) tracker: TaskTracker,
}

impl<H: HttpTransport, C: Clock> Server for KnotSshServer<H, C> {
    type Handler = KnotSession<H, C>;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Self::Handler {
        KnotSession::new(
            Arc::clone(&self.state),
            self.tracker.clone(),
            peer.map(|addr| addr.ip()),
        )
    }
}

pub(crate) struct KnotSession<H, C> {
    state: Arc<SshState<H, C>>,
    tracker: TaskTracker,
    key: Option<OfferedKey>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    protocols: HashSet<ChannelId>,
    peer: Option<IpAddr>,
}

impl<H, C> KnotSession<H, C> {
    fn new(state: Arc<SshState<H, C>>, tracker: TaskTracker, peer: Option<IpAddr>) -> Self {
        Self {
            state,
            tracker,
            key: None,
            channels: HashMap::new(),
            protocols: HashSet::new(),
            peer,
        }
    }
}

impl<H: HttpTransport, C: Clock> Handler for KnotSession<H, C> {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let reject = Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        };
        let Ok(blob) = public_key.to_bytes() else {
            return Ok(reject);
        };
        let key = OfferedKey::from_bytes(blob);
        if self
            .state
            .roster
            .recognizes_fresh(&key, &self.state.index, &self.state.atproto)
            .await
        {
            self.key = Some(key);
            Ok(Auth::Accept)
        } else {
            Ok(reject)
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.channels.insert(channel.id(), channel);
        Ok(true)
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if variable_name == "GIT_PROTOCOL"
            && variable_value
                .split(':')
                .any(|token| token.trim() == "version=2")
        {
            self.protocols.insert(channel);
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.protocols.remove(&channel);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        self.protocols.remove(&channel);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(handle) = self.channels.remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let state = Arc::clone(&self.state);
        let key = self.key.clone();
        self.tracker.spawn(async move {
            crate::exec::run_greeting(state, key, handle).await;
        });
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(handle) = self.channels.remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let protocol_v2 = self.protocols.remove(&channel);
        let state = Arc::clone(&self.state);
        let key = self.key.clone();
        let peer = self.peer;
        let command = data.to_vec();
        self.tracker.spawn(async move {
            run_exec(state, key, handle, &command, protocol_v2, peer).await;
        });
        Ok(())
    }
}
