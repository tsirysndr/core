package oauth

import (
	"encoding/json"
	"errors"
	"net/http"
	"time"
)

const MaxAccounts = 20

var ErrMaxAccountsReached = errors.New("maximum number of linked accounts reached")

type AccountInfo struct {
	Did       string `json:"did"`
	Handle    string `json:"handle"`
	SessionId string `json:"session_id"`
	AddedAt   int64  `json:"added_at"`
}

type AccountRegistry struct {
	Accounts []AccountInfo `json:"accounts"`
}

type MultiAccountUser struct {
	Active   User
	Accounts []AccountInfo
}

func (m *MultiAccountUser) Did() string {
	return m.Active.Did
}

func (o *OAuth) GetAccounts(r *http.Request) *AccountRegistry {
	session, err := o.SessStore.Get(r, AccountsName)
	if err != nil || session.IsNew {
		return &AccountRegistry{Accounts: []AccountInfo{}}
	}

	data, ok := session.Values["accounts"].(string)
	if !ok {
		return &AccountRegistry{Accounts: []AccountInfo{}}
	}

	var registry AccountRegistry
	if err := json.Unmarshal([]byte(data), &registry); err != nil {
		return &AccountRegistry{Accounts: []AccountInfo{}}
	}

	return &registry
}

func (o *OAuth) saveAccounts(w http.ResponseWriter, r *http.Request, registry *AccountRegistry) error {
	session, err := o.SessStore.Get(r, AccountsName)
	if err != nil {
		o.Logger.Warn("failed to decode existing accounts cookie, will create new", "err", err)
	}

	data, err := json.Marshal(registry)
	if err != nil {
		return err
	}

	session.Values["accounts"] = string(data)
	session.Options.MaxAge = 60 * 60 * 24 * 365
	session.Options.HttpOnly = true
	session.Options.Secure = !o.Config.Core.Dev
	session.Options.SameSite = http.SameSiteLaxMode

	return session.Save(r, w)
}

func (r *AccountRegistry) AddAccount(did, handle, sessionId string) error {
	for i, acc := range r.Accounts {
		if acc.Did == did {
			r.Accounts[i].SessionId = sessionId
			r.Accounts[i].Handle = handle
			return nil
		}
	}

	if len(r.Accounts) >= MaxAccounts {
		return ErrMaxAccountsReached
	}

	r.Accounts = append(r.Accounts, AccountInfo{
		Did:       did,
		Handle:    handle,
		SessionId: sessionId,
		AddedAt:   time.Now().Unix(),
	})
	return nil
}

func (r *AccountRegistry) RemoveAccount(did string) {
	filtered := make([]AccountInfo, 0, len(r.Accounts))
	for _, acc := range r.Accounts {
		if acc.Did != did {
			filtered = append(filtered, acc)
		}
	}
	r.Accounts = filtered
}

func (r *AccountRegistry) FindAccount(did string) *AccountInfo {
	for i := range r.Accounts {
		if r.Accounts[i].Did == did {
			return &r.Accounts[i]
		}
	}
	return nil
}

func (o *OAuth) GetMultiAccountUser(r *http.Request) *MultiAccountUser {
	sess, err := o.ResumeSession(r)
	if err != nil {
		return nil
	}

	registry := o.GetAccounts(r)
	return &MultiAccountUser{
		Active: User{
			Did: sess.Data.AccountDID.String(),
		},
		Accounts: registry.Accounts,
	}
}

type AuthReturnInfo struct {
	ReturnURL  string
	AddAccount bool
}

func (o *OAuth) SetAuthReturn(w http.ResponseWriter, r *http.Request, returnURL string, addAccount bool) error {
	session, err := o.SessStore.Get(r, AuthReturnName)
	if err != nil {
		return err
	}

	session.Values[AuthReturnURL] = returnURL
	session.Values[AuthAddAccount] = addAccount
	session.Options.MaxAge = 60 * 30
	session.Options.HttpOnly = true
	session.Options.Secure = !o.Config.Core.Dev
	session.Options.SameSite = http.SameSiteLaxMode

	return session.Save(r, w)
}

func (o *OAuth) GetAuthReturn(r *http.Request) *AuthReturnInfo {
	session, err := o.SessStore.Get(r, AuthReturnName)
	if err != nil || session.IsNew {
		return &AuthReturnInfo{}
	}

	returnURL, _ := session.Values[AuthReturnURL].(string)
	addAccount, _ := session.Values[AuthAddAccount].(bool)

	return &AuthReturnInfo{
		ReturnURL:  returnURL,
		AddAccount: addAccount,
	}
}

func (o *OAuth) ClearAuthReturn(w http.ResponseWriter, r *http.Request) error {
	session, err := o.SessStore.Get(r, AuthReturnName)
	if err != nil {
		return err
	}

	session.Options.MaxAge = -1
	return session.Save(r, w)
}
