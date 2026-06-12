use once_cell::sync::Lazy;
use prost::Message as ProstMessage;
use prost_reflect::DescriptorPool;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod v1 {
    include!("gen/spindle/agent/v1/spindle.agent.v1.rs");
}

pub use v1::Message;

pub static DESCRIPTOR_POOL: Lazy<DescriptorPool> = Lazy::new(|| {
    let bytes = include_bytes!("gen/file_descriptor_set.bin");
    DescriptorPool::decode(&bytes[..]).unwrap()
});

macro_rules! impl_reflect {
    ($($t:ident),* $(,)?) => {
        $(
            impl prost_reflect::ReflectMessage for v1::$t {
                fn descriptor(&self) -> prost_reflect::MessageDescriptor {
                    DESCRIPTOR_POOL
                        .get_message_by_name(concat!("spindle.agent.v1.", stringify!($t)))
                        .unwrap()
                }
            }
        )*
    };
}

impl_reflect!(
    Hello,
    Init,
    ExecStart,
    ExecStdout,
    ExecStderr,
    ExecExit,
    ActivateConfig,
    ActivateConfigResult,
    BuiltPaths,
    CacheDrain,
    CacheDrainResult,
    Poweroff,
    PoweroffResult,
    Message,
);

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u32 = 10240;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[macro_export]
macro_rules! on_payload {
    (ref $msg:expr, { $( $field:ident => $body:expr ),* $(,)? }) => {
        #[allow(unused_variables)]
        $(if let Some(ref $field) = $msg.$field { Some($body) } else)* { None }
    };
    ($msg:expr, { $( $field:ident => $body:expr ),* $(,)? }) => {
        $(if let Some($field) = $msg.$field { Some($body) } else )* { None }
    };
}

pub fn kind(msg: &Message) -> &'static str {
    // todo(dawn): maybe eventually we should have a custom protoc plugin for
    // generating an enum, right now not worth it, when we have more needs for
    // it imo we can consider it again
    on_payload!(ref msg, {
        hello => "hello",
        init => "init",
        exec_start => "exec_start",
        exec_stdout => "exec_stdout",
        exec_stderr => "exec_stderr",
        exec_exit => "exec_exit",
        activate_config => "activate_config",
        activate_config_result => "activate_config_result",
        built_paths => "built_paths",
        cache_drain => "cache_drain",
        cache_drain_result => "cache_drain_result",
        poweroff => "poweroff",
        poweroff_result => "poweroff_result",
    })
    .unwrap_or_else(|| unreachable!("validated message has no payload"))
}

pub fn error_or_empty(error: Option<String>) -> String {
    error.filter(|error| !error.is_empty()).unwrap_or_default()
}

pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, msg: &Message) -> io::Result<()> {
    if let Err(err) = prost_protovalidate::validate(msg) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("validate agent message: {err}"),
        ));
    }

    let mut data = Vec::new();
    msg.encode(&mut data)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if data.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("agent message exceeded {MAX_MESSAGE_BYTES} bytes"),
        ));
    }

    writer.write_all(&(data.len() as u32).to_be_bytes()).await?;
    writer.write_all(&data).await?;
    writer.flush().await
}

pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Message>> {
    let Some(header) = read_header(reader).await? else {
        return Ok(None);
    };
    let size = u32::from_be_bytes(header) as usize;
    if size > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("agent message exceeded {MAX_MESSAGE_BYTES} bytes"),
        ));
    }

    let mut data = vec![0; size];
    reader.read_exact(&mut data).await?;
    let msg = Message::decode(&data[..])
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    if let Err(err) = prost_protovalidate::validate(&msg) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("validate agent message: {err}"),
        ));
    }

    Ok(Some(msg))
}

async fn read_header<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<[u8; 4]>> {
    let mut header = [0; 4];
    let mut read = 0;
    while read < header.len() {
        match reader.read(&mut header[read..]).await {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial agent message header",
                ));
            }
            Ok(n) => read += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(Some(header))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_protobuf_message() {
        let msg = Message {
            id: "built-paths".to_owned(),
            built_paths: Some(v1::BuiltPaths {
                paths: vec!["/nix/store/abc-package".to_owned()],
                reason: "post_build_hook".to_owned(),
            }),
            ..Default::default()
        };

        let mut encoded = Vec::new();
        write_message(&mut encoded, &msg).await.unwrap();

        let decoded = read_message(&mut &encoded[..]).await.unwrap().unwrap();
        assert!(decoded.built_paths.is_some());
        if let Some(p) = decoded.built_paths {
            assert_eq!(p.paths, ["/nix/store/abc-package"]);
            assert_eq!(p.reason, "post_build_hook");
        }
    }

    #[test]
    fn validates_messages() {
        // 1. valid message (exactly one field set)
        let valid = Message {
            id: "test-1".to_owned(),
            hello: Some(v1::Hello::default()),
            ..Default::default()
        };
        assert!(prost_protovalidate::validate(&valid).is_ok());

        // 2. invalid message (zero fields set)
        let invalid_zero = Message {
            id: "test-2".to_owned(),
            ..Default::default()
        };
        assert!(prost_protovalidate::validate(&invalid_zero).is_err());

        // 3. invalid message (multiple fields set)
        let invalid_multi = Message {
            id: "test-3".to_owned(),
            hello: Some(v1::Hello::default()),
            init: Some(v1::Init::default()),
            ..Default::default()
        };
        assert!(prost_protovalidate::validate(&invalid_multi).is_err());
    }
}
