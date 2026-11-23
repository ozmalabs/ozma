// SPDX-License-Identifier: AGPL-3.0-only
//! Linux PAM authentication for local OS accounts.
//!
//! The provider is deliberately small: a service name, an allow policy, and a
//! non-interactive conversation that only ever supplies the caller's password
//! (and the username for echo-on prompts). It never writes a password to a log,
//! a `Debug` implementation, or an error detail.
//!
//! PAM has a C ABI and no stable safe Rust wrapper is available in this
//! workspace, so the FFI lives in the private `sys` module. That module is
//! the **only** place in this crate allowed to use `unsafe`, and every unsafe
//! block has a safety comment. On non-Linux targets the same public API is
//! compiled against a stub that reports `auth.pam_unavailable`, so a portable
//! build never links libpam.
//!
//! The real-PAM test is opt-in because it talks to the host's account database:
//! set `OZMA_TEST_PAM=1` to exercise a wrong-password rejection. It does not
//! prompt and it does not need a valid password.

use std::collections::BTreeSet;
use std::path::Path;

use ozma_protocol::{ApiError, PermissionSet, Result};

use crate::credential::{AuthenticatedIdentity, Credential, LocalAuthenticator};
use crate::error::codes;

/// Preferred PAM service name used by Ozma.
pub const DEFAULT_PAM_SERVICE: &str = "ozma";
/// PAM service used when `/etc/pam.d/ozma` is not installed.
pub const FALLBACK_PAM_SERVICE: &str = "login";

/// Which local accounts may authenticate through PAM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserPolicy {
    /// Only the account that owns the node process (the default).
    CurrentUser(String),
    /// An explicit allow list. An empty list denies every account.
    AllowList(BTreeSet<String>),
}

/// Configuration for [`PamAuthenticator`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PamConfig {
    service: String,
    policy: UserPolicy,
    permissions: PermissionSet,
}

impl PamConfig {
    /// Build a config for the process owner.
    ///
    /// The PAM service defaults to `ozma` when it is installed, otherwise to
    /// `login`. Returns `auth.pam_config` when the current account cannot be
    /// determined, rather than silently allowing an empty policy.
    pub fn new() -> Result<Self> {
        let user = current_user().ok_or_else(|| {
            ApiError::new(
                codes::PAM_CONFIG,
                "cannot determine the account that owns this node process",
            )
            .with_remediation("configure an explicit user policy before enabling PAM")
        })?;
        Ok(Self::for_user(user))
    }

    /// Build a config that only allows `user` (primarily for tests and for a
    /// service manager that knows the account explicitly).
    pub fn for_user(user: impl Into<String>) -> Self {
        Self {
            service: default_service(),
            policy: UserPolicy::CurrentUser(user.into()),
            permissions: PermissionSet::all(),
        }
    }

    /// Use an explicit PAM service instead of the `ozma`/`login` default.
    pub fn with_service(mut self, service: impl Into<String>) -> Self {
        self.service = service.into();
        self
    }

    /// Replace the policy with an explicit allow list.
    pub fn with_allowed_users<I, S>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.policy = UserPolicy::AllowList(users.into_iter().map(Into::into).collect());
        self
    }

    /// Restrict the permissions granted after a successful login.
    pub fn with_permissions(mut self, permissions: PermissionSet) -> Self {
        self.permissions = permissions;
        self
    }

    /// The PAM service name that will be passed to `pam_start`.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// The policy in force.
    pub fn policy(&self) -> &UserPolicy {
        &self.policy
    }

    /// Permissions granted to a successful identity.
    pub fn permissions(&self) -> &PermissionSet {
        &self.permissions
    }

    /// Whether `principal` is allowed to reach PAM at all.
    pub fn is_user_allowed(&self, principal: &str) -> bool {
        if !valid_username(principal) {
            return false;
        }
        match &self.policy {
            UserPolicy::CurrentUser(user) => principal == user,
            UserPolicy::AllowList(users) => users.contains(principal),
        }
    }

    /// Typed policy check used by the authenticator.
    ///
    /// A denied principal is reported as `auth.pam_rejected` (never as
    /// "unknown user") so the provider does not disclose account existence.
    pub fn check_policy(&self, principal: &str) -> Result<()> {
        if self.is_user_allowed(principal) {
            return Ok(());
        }
        Err(ApiError::new(
            codes::PAM_REJECTED,
            "PAM authentication is not allowed for this local account",
        )
        .with_details(serde_json::json!({ "principal": principal })))
    }
}

/// A [`LocalAuthenticator`] backed by the host's PAM stack.
///
/// The password is copied into PAM's conversation buffer for the duration of
/// the call only. No method on this type logs it.
#[derive(Clone, Debug)]
pub struct PamAuthenticator {
    config: PamConfig,
}

impl PamAuthenticator {
    /// Create a PAM authenticator for the process owner using the default
    /// service.
    pub fn new() -> Result<Self> {
        Ok(Self::with_config(PamConfig::new()?))
    }

    /// Create an authenticator from an explicit configuration.
    pub fn with_config(config: PamConfig) -> Self {
        Self { config }
    }

    /// The configuration in force.
    pub fn config(&self) -> &PamConfig {
        &self.config
    }

    /// Expose the policy check without invoking PAM.
    pub fn check_policy(&self, principal: &str) -> Result<()> {
        self.config.check_policy(principal)
    }
}

impl LocalAuthenticator for PamAuthenticator {
    fn authenticate(
        &self,
        principal: &str,
        credential: Credential<'_>,
    ) -> Result<AuthenticatedIdentity> {
        self.config.check_policy(principal)?;

        let password = match credential {
            Credential::Password(password) => password,
            Credential::SshPublicKey(_) => {
                return Err(ApiError::new(
                    codes::PAM_CONFIG,
                    "the PAM provider only accepts a password credential",
                ));
            }
        };

        if password.is_empty() {
            return Err(ApiError::new(
                codes::PAM_REJECTED,
                "PAM rejected an empty password",
            ));
        }

        sys::authenticate(&self.config.service, principal, password)?;

        Ok(AuthenticatedIdentity {
            principal: principal.to_owned(),
            permissions: self.config.permissions.clone(),
        })
    }
}

/// Resolve the default PAM service, preferring `ozma` and falling back to
/// `login` when its policy file is absent.
pub fn default_service() -> String {
    resolve_service(DEFAULT_PAM_SERVICE, FALLBACK_PAM_SERVICE, |name| {
        Path::new("/etc/pam.d").join(name).is_file()
    })
}

fn resolve_service(preferred: &str, fallback: &str, exists: impl Fn(&str) -> bool) -> String {
    if exists(preferred) {
        preferred.to_owned()
    } else {
        fallback.to_owned()
    }
}

/// Conservative POSIX account-name validation before a name reaches PAM.
fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && !name
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | ':'))
}

fn current_user() -> Option<String> {
    sys::current_user().or_else(|| {
        std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .filter(|user| valid_username(user))
    })
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use ozma_protocol::{ApiError, Result};

    use crate::error::codes;

    pub(super) fn current_user() -> Option<String> {
        None
    }

    pub(super) fn authenticate(_service: &str, _principal: &str, _password: &str) -> Result<()> {
        Err(ApiError::new(
            codes::PAM_UNAVAILABLE,
            "PAM authentication is only available on Linux",
        )
        .with_remediation("build and run the node on Linux with libpam installed"))
    }
}

/// Isolated PAM FFI. Nothing outside this module uses `unsafe`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod sys {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_void};
    use std::ptr;

    use ozma_protocol::{ApiError, Result};

    use crate::error::codes;

    // Linux-PAM return codes from <security/_pam_types.h>.
    const PAM_SUCCESS: c_int = 0;
    const PAM_OPEN_ERR: c_int = 1;
    const PAM_SYMBOL_ERR: c_int = 2;
    const PAM_SERVICE_ERR: c_int = 3;
    const PAM_SYSTEM_ERR: c_int = 4;
    const PAM_BUF_ERR: c_int = 5;
    const PAM_PERM_DENIED: c_int = 6;
    const PAM_AUTH_ERR: c_int = 7;
    const PAM_CRED_INSUFFICIENT: c_int = 8;
    const PAM_AUTHINFO_UNAVAIL: c_int = 9;
    const PAM_USER_UNKNOWN: c_int = 10;
    const PAM_MAXTRIES: c_int = 11;
    const PAM_NEW_AUTHTOK_REQD: c_int = 12;
    const PAM_ACCT_EXPIRED: c_int = 13;
    const PAM_CONV_ERR: c_int = 19;
    const PAM_AUTHTOK_ERR: c_int = 20;
    const PAM_ABORT: c_int = 26;

    // Conversation styles from <security/_pam_types.h>.
    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    const PAM_PROMPT_ECHO_ON: c_int = 2;
    const PAM_ERROR_MSG: c_int = 3;
    const PAM_TEXT_INFO: c_int = 4;

    /// A hard cap on conversation messages keeps a hostile PAM module from
    /// asking us to allocate an unbounded response array.
    const MAX_CONV_MESSAGES: c_int = 32;

    #[repr(C)]
    struct PamMessage {
        msg_style: c_int,
        msg: *const c_char,
    }

    #[repr(C)]
    struct PamResponse {
        resp: *mut c_char,
        resp_retcode: c_int,
    }

    #[repr(C)]
    struct PamConv {
        conv: Option<
            unsafe extern "C" fn(
                num_msg: c_int,
                msg: *mut *const PamMessage,
                resp: *mut *mut PamResponse,
                appdata_ptr: *mut c_void,
            ) -> c_int,
        >,
        appdata_ptr: *mut c_void,
    }

    #[link(name = "pam")]
    extern "C" {
        fn pam_start(
            service_name: *const c_char,
            user: *const c_char,
            pam_conv: *const PamConv,
            pamh: *mut *mut c_void,
        ) -> c_int;
        fn pam_authenticate(pamh: *mut c_void, flags: c_int) -> c_int;
        fn pam_acct_mgmt(pamh: *mut c_void, flags: c_int) -> c_int;
        fn pam_end(pamh: *mut c_void, pam_status: c_int) -> c_int;
        fn pam_strerror(pamh: *mut c_void, errnum: c_int) -> *const c_char;
    }

    /// Everything the conversation callback is allowed to read. It lives on
    /// the stack for the duration of the synchronous PAM call.
    struct ConvData {
        username: CString,
        password: CString,
    }

    unsafe extern "C" fn conversation(
        num_msg: c_int,
        msg: *mut *const PamMessage,
        resp: *mut *mut PamResponse,
        appdata_ptr: *mut c_void,
    ) -> c_int {
        if resp.is_null() || appdata_ptr.is_null() || msg.is_null() || num_msg <= 0 {
            return PAM_CONV_ERR;
        }
        if num_msg > MAX_CONV_MESSAGES {
            return PAM_CONV_ERR;
        }

        // SAFETY: `appdata_ptr` points at the `ConvData` owned by
        // `authenticate` for the whole synchronous `pam_*` call. PAM does not
        // retain it after the call returns.
        let data = unsafe { &*(appdata_ptr as *const ConvData) };

        // SAFETY: `calloc` returns a zeroed array of `num_msg` responses that
        // matches the C layout of `struct pam_response`.
        let responses =
            unsafe { libc::calloc(num_msg as usize, std::mem::size_of::<PamResponse>()) }
                as *mut PamResponse;
        if responses.is_null() {
            return PAM_BUF_ERR;
        }

        for index in 0..num_msg {
            // SAFETY: PAM passes an array of `num_msg` message pointers.
            let message = unsafe { *msg.offset(index as isize) };
            if message.is_null() {
                unsafe { free_responses(responses, num_msg) };
                return PAM_CONV_ERR;
            }

            // SAFETY: `message` is a valid, PAM-owned `pam_message` for the
            // duration of this callback.
            let style = unsafe { (*message).msg_style };
            let source = match style {
                PAM_PROMPT_ECHO_OFF => data.password.as_ptr(),
                PAM_PROMPT_ECHO_ON => data.username.as_ptr(),
                PAM_ERROR_MSG | PAM_TEXT_INFO => ptr::null(),
                _ => {
                    unsafe { free_responses(responses, num_msg) };
                    return PAM_CONV_ERR;
                }
            };

            if !source.is_null() {
                // SAFETY: `source` is a NUL-terminated string and `strdup`
                // copies it into memory that PAM owns after we return.
                let copy = unsafe { libc::strdup(source) };
                if copy.is_null() {
                    unsafe { free_responses(responses, num_msg) };
                    return PAM_BUF_ERR;
                }
                // SAFETY: `responses` has `num_msg` entries and `index` is in
                // range.
                unsafe { (*responses.offset(index as isize)).resp = copy };
            }
            // SAFETY: same bounds argument as above.
            unsafe { (*responses.offset(index as isize)).resp_retcode = 0 };
        }

        // SAFETY: `resp` is a valid out-parameter supplied by PAM.
        unsafe { *resp = responses };
        PAM_SUCCESS
    }

    /// Free a partially built response array after a conversation failure.
    unsafe fn free_responses(responses: *mut PamResponse, num_msg: c_int) {
        for index in 0..num_msg {
            // SAFETY: `responses` has `num_msg` entries; `resp` is either null
            // or a `strdup` allocation that nobody else owns yet.
            unsafe {
                let resp = (*responses.offset(index as isize)).resp;
                if !resp.is_null() {
                    libc::free(resp as *mut c_void);
                }
            }
        }
        // SAFETY: `responses` came from `calloc` and has not been freed.
        unsafe { libc::free(responses as *mut c_void) };
    }

    struct PamHandle {
        raw: *mut c_void,
        status: c_int,
    }

    impl PamHandle {
        fn start(
            service: &str,
            user: &str,
            conv: *const PamConv,
        ) -> std::result::Result<Self, ApiError> {
            let service = CString::new(service).map_err(|_| {
                ApiError::new(
                    codes::PAM_CONFIG,
                    "the PAM service name contains a NUL byte",
                )
            })?;
            let user = CString::new(user).map_err(|_| {
                ApiError::new(codes::PAM_CONFIG, "the PAM username contains a NUL byte")
            })?;

            let mut raw: *mut c_void = ptr::null_mut();
            // SAFETY: all three pointers are valid for the duration of the
            // call; `raw` is a valid out-parameter.
            let status = unsafe { pam_start(service.as_ptr(), user.as_ptr(), conv, &mut raw) };
            if status != PAM_SUCCESS || raw.is_null() {
                if !raw.is_null() {
                    // SAFETY: `pam_start` returned a handle even though it
                    // reported failure; end it to avoid leaking module state.
                    unsafe {
                        pam_end(raw, status);
                    }
                }
                return Err(start_error(status));
            }
            Ok(Self {
                raw,
                status: PAM_SUCCESS,
            })
        }

        fn finish(mut self) -> Result<()> {
            self.status = PAM_SUCCESS;
            Ok(())
        }

        fn fail(&mut self, status: c_int) -> ApiError {
            self.status = status;
            let detail = self.strerror(status);
            map_failure(status, detail)
        }

        fn strerror(&self, status: c_int) -> String {
            // SAFETY: the handle is live and `pam_strerror` returns either a
            // NUL-terminated static/module string or NULL.
            let ptr = unsafe { pam_strerror(self.raw, status) };
            if ptr.is_null() {
                return format!("PAM error {status}");
            }
            // SAFETY: a non-NULL return is a NUL-terminated C string valid
            // until the next PAM call.
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }

    impl Drop for PamHandle {
        fn drop(&mut self) {
            if !self.raw.is_null() {
                // SAFETY: the handle came from `pam_start` and is only ended
                // here. Passing the last status is the documented cleanup
                // convention.
                unsafe {
                    pam_end(self.raw, self.status);
                }
                self.raw = ptr::null_mut();
            }
        }
    }

    pub(super) fn authenticate(service: &str, principal: &str, password: &str) -> Result<()> {
        let password = CString::new(password).map_err(|_| {
            ApiError::new(
                codes::PAM_REJECTED,
                "PAM rejected a password containing a NUL byte",
            )
        })?;
        let username = CString::new(principal).map_err(|_| {
            ApiError::new(codes::PAM_CONFIG, "the PAM username contains a NUL byte")
        })?;
        let data = ConvData { username, password };
        let conv = PamConv {
            conv: Some(conversation),
            // SAFETY: `data` lives on this stack frame until every PAM call
            // that can invoke the callback has returned.
            appdata_ptr: &data as *const ConvData as *mut c_void,
        };

        let mut handle = PamHandle::start(service, principal, &conv)?;

        // SAFETY: `handle.raw` is a live PAM handle and the conversation
        // callback only touches `data`, which is still alive.
        let status = unsafe { pam_authenticate(handle.raw, 0) };
        if status != PAM_SUCCESS {
            return Err(handle.fail(status));
        }

        // SAFETY: same live handle as above.
        let status = unsafe { pam_acct_mgmt(handle.raw, 0) };
        if status != PAM_SUCCESS {
            return Err(handle.fail(status));
        }

        handle.finish()
    }

    fn start_error(status: c_int) -> ApiError {
        let code = match status {
            PAM_BUF_ERR => codes::PAM_UNAVAILABLE,
            _ => codes::PAM_CONFIG,
        };
        let error = ApiError::new(
            code,
            format!("PAM could not start the configured service (error {status})"),
        );
        if status == PAM_SERVICE_ERR || status == PAM_SYSTEM_ERR || status == PAM_OPEN_ERR {
            error.with_remediation(
                "install the PAM service file under /etc/pam.d or select an existing service",
            )
        } else {
            error
        }
    }

    fn map_failure(status: c_int, detail: String) -> ApiError {
        let code = match status {
            PAM_AUTH_ERR
            | PAM_PERM_DENIED
            | PAM_USER_UNKNOWN
            | PAM_MAXTRIES
            | PAM_NEW_AUTHTOK_REQD
            | PAM_ACCT_EXPIRED
            | PAM_AUTHTOK_ERR
            | PAM_CRED_INSUFFICIENT
            | PAM_ABORT => codes::PAM_REJECTED,
            PAM_AUTHINFO_UNAVAIL | PAM_BUF_ERR | PAM_CONV_ERR | PAM_SERVICE_ERR
            | PAM_SYSTEM_ERR | PAM_OPEN_ERR | PAM_SYMBOL_ERR => codes::PAM_UNAVAILABLE,
            _ => codes::PAM_REJECTED,
        };

        let error = ApiError::new(code, detail);
        if status == PAM_NEW_AUTHTOK_REQD {
            error.with_remediation("the local account must change its password before use")
        } else if status == PAM_ACCT_EXPIRED {
            error.with_remediation("the local account has expired")
        } else {
            error
        }
    }

    pub(super) fn current_user() -> Option<String> {
        // SAFETY: `geteuid` has no preconditions.
        let uid = unsafe { libc::geteuid() };

        // `passwd` is a plain C struct of integers and pointers; an all-zero
        // value is a valid initializer before `getpwuid_r` fills it.
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as c_char; 16 * 1024];
        let mut result: *mut libc::passwd = ptr::null_mut();

        // SAFETY: all pointers refer to live, correctly sized local storage.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &mut passwd,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut result,
            )
        };

        if status == 0 && !result.is_null() && !passwd.pw_name.is_null() {
            // SAFETY: on success `pw_name` points into `buffer`, which is
            // still alive, and is NUL-terminated.
            let name = unsafe { CStr::from_ptr(passwd.pw_name) }
                .to_string_lossy()
                .into_owned();
            if !name.is_empty() {
                return Some(name);
            }
        }
        None
    }

    // Keep the constants used by the mapping next to the mapping itself.
    const _: () = {
        assert!(PAM_SUCCESS == 0);
        assert!(PAM_AUTH_ERR == 7);
        assert!(PAM_ACCT_EXPIRED == 13);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_prefers_installed_ozma_and_falls_back_to_login() {
        assert_eq!(
            resolve_service(DEFAULT_PAM_SERVICE, FALLBACK_PAM_SERVICE, |name| name
                == "ozma"),
            "ozma"
        );
        assert_eq!(
            resolve_service(DEFAULT_PAM_SERVICE, FALLBACK_PAM_SERVICE, |_| false),
            "login"
        );
    }

    #[test]
    fn default_policy_allows_only_the_configured_account() {
        let config = PamConfig::for_user("alice");
        assert!(config.is_user_allowed("alice"));
        assert!(!config.is_user_allowed("bob"));
        assert!(!config.is_user_allowed(""));
        assert_eq!(
            config.check_policy("bob").unwrap_err().code,
            codes::PAM_REJECTED
        );
    }

    #[test]
    fn allow_list_replaces_the_default_policy() {
        let config = PamConfig::for_user("alice").with_allowed_users(["alice", "bob"]);
        assert!(config.is_user_allowed("alice"));
        assert!(config.is_user_allowed("bob"));
        assert!(!config.is_user_allowed("carol"));

        let empty = PamConfig::for_user("alice").with_allowed_users(Vec::<String>::new());
        assert!(!empty.is_user_allowed("alice"));
    }

    #[test]
    fn explicit_service_and_permissions_are_preserved() {
        let permissions = PermissionSet::view_only();
        let config = PamConfig::for_user("alice")
            .with_service("custom-ozma")
            .with_permissions(permissions.clone());
        assert_eq!(config.service(), "custom-ozma");
        assert_eq!(config.permissions(), &permissions);
        assert_eq!(
            config.policy(),
            &UserPolicy::CurrentUser("alice".to_owned())
        );
    }

    #[test]
    fn rejects_unsafe_usernames_before_pam() {
        let config = PamConfig::for_user("alice");
        for name in [
            "",
            " alice",
            "alice ",
            "alice\nroot",
            "alice/root",
            "-alice",
            "a:b",
        ] {
            assert!(
                !config.is_user_allowed(name),
                "unexpectedly allowed {name:?}"
            );
            assert_eq!(
                config.check_policy(name).unwrap_err().code,
                codes::PAM_REJECTED
            );
        }
    }

    #[test]
    fn non_password_credentials_are_a_configuration_error() {
        let auth = PamAuthenticator::with_config(PamConfig::for_user("alice"));
        let error = auth
            .authenticate("alice", Credential::SshPublicKey(b"ssh-ed25519 AAAA"))
            .unwrap_err();
        assert_eq!(error.code, codes::PAM_CONFIG);
    }

    #[test]
    fn denied_users_never_reach_pam() {
        let auth = PamAuthenticator::with_config(
            PamConfig::for_user("alice").with_allowed_users(["alice"]),
        );
        let error = auth
            .authenticate("bob", Credential::Password("irrelevant"))
            .unwrap_err();
        assert_eq!(error.code, codes::PAM_REJECTED);
    }

    #[test]
    fn empty_passwords_are_rejected_without_pam() {
        let auth = PamAuthenticator::with_config(PamConfig::for_user("alice"));
        let error = auth
            .authenticate("alice", Credential::Password(""))
            .unwrap_err();
        assert_eq!(error.code, codes::PAM_REJECTED);
    }

    #[test]
    fn configuration_does_not_debug_passwords() {
        let auth = PamAuthenticator::with_config(PamConfig::for_user("alice"));
        let shown = format!("{auth:?}");
        assert!(shown.contains("alice"));
        assert!(!shown.contains("password"));
    }

    #[test]
    fn real_pam_rejects_a_wrong_password_when_requested() {
        if std::env::var("OZMA_TEST_PAM").as_deref() != Ok("1") {
            eprintln!("skipping real PAM test; set OZMA_TEST_PAM=1 to run it");
            return;
        }
        let user = current_user().expect("a current user is required for the PAM test");
        let auth = PamAuthenticator::with_config(PamConfig::for_user(user.clone()));
        let error = auth
            .authenticate(
                &user,
                Credential::Password("ozma-test-password-that-is-not-valid-7f3a"),
            )
            .expect_err("a deliberately wrong password must not authenticate");
        assert_eq!(
            error.code,
            codes::PAM_REJECTED,
            "unexpected PAM failure: {error}"
        );
        eprintln!(
            "real PAM test ran against service {}",
            auth.config().service()
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_reports_unavailable() {
        let auth = PamAuthenticator::with_config(PamConfig::for_user("alice"));
        let error = auth
            .authenticate("alice", Credential::Password("something"))
            .unwrap_err();
        assert_eq!(error.code, codes::PAM_UNAVAILABLE);
    }
}
