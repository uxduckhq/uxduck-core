//! The daemon's credential store: a JSON file readable only by the user
//! (0600) in the data directory, behind the engine's `Credentials` port. The
//! OS keychain can replace it behind the same port (review.md S3).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

static WRITE: Mutex<()> = Mutex::new(());

use uxduck_engine::runtime::Credentials;

#[derive(Clone, Debug)]
pub struct FileCredentials {
    path: PathBuf,
    api: Option<String>,
    legacy_api: Option<String>,
}

impl FileCredentials {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("credentials.json"),
            api: crate::dev_api::api_url(),
            legacy_api: crate::dev_api::legacy_api_url(),
        }
    }

    fn load(&self) -> Result<BTreeMap<String, String>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("Cannot read {}: {e}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(format!("Cannot read {}: {error}", self.path.display())),
        }
    }

    /// Read fresh on every call, so a key set a moment ago is used at once.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        let mut all = self.load().ok()?;
        if name == uxduck_engine::ACCOUNT_TOKEN {
            let api = self.api.as_deref()?;
            let token = all.remove(&account_key(api)).or_else(|| {
                (self.api == self.legacy_api)
                    .then(|| all.remove(name))
                    .flatten()
            })?;
            // Shared local data never follows a different person's sign-in.
            let owner = all.get(&self.scoped("workspace.owner"));
            let person = all.get(&self.scoped("account.person"));
            if owner.is_some() && owner != person {
                return None;
            }
            if owner.is_none()
                && all
                    .get(&self.scoped("workspace.legacy"))
                    .is_none_or(|pending| *pending != fingerprint(&token))
            {
                return None;
            }
            return Some(token);
        }
        all.remove(name)
    }

    fn scoped(&self, name: &str) -> String {
        format!(
            "{name}:{}",
            self.api.as_deref().unwrap_or("none").trim_end_matches('/')
        )
    }

    /// The computer's sign-in, whichever operator is chosen (ADR-0033, one
    /// account per computer). Background data services use `get(TOKEN)`
    /// instead, which additionally checks the dataset's immutable owner.
    pub fn account_session(&self) -> Option<String> {
        self.api.as_ref()?;
        let all = self.load().ok()?;
        all.get(&self.scoped("account.token"))
            .cloned()
            .or_else(|| {
                (self.api == self.legacy_api)
                    .then(|| all.get(uxduck_engine::ACCOUNT_TOKEN).cloned())
                    .flatten()
            })
            .filter(|token| !token.is_empty())
    }

    /// Preserve evidence of the legacy workspace before another account can sign in.
    /// An unreachable server must not make the next sign-in its new owner.
    pub fn protect_legacy_workspace(&self) -> Result<(), String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all = self.load()?;
        if !all.contains_key(&self.scoped("workspace.owner"))
            && !all.contains_key(&self.scoped("workspace.legacy"))
            && let Some(token) = self.account_session()
        {
            all.insert(self.scoped("workspace.legacy"), fingerprint(&token));
            self.save(&all).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Identity comes from the authenticated /v1/me response, never email input.
    pub fn remember_person(&self, token: &str, id: &str) -> Result<bool, String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        if self.account_session().as_deref() != Some(token) {
            return Ok(false);
        }
        let mut all = self.load()?;
        all.insert(self.scoped("account.person"), id.into());
        let owner = self.scoped("workspace.owner");
        let legacy = self.scoped("workspace.legacy");
        if !all.contains_key(&owner)
            && all
                .get(&legacy)
                .is_none_or(|old| *old == fingerprint(token))
        {
            all.insert(owner.clone(), id.into());
            all.remove(&legacy);
        }
        let owns = all.get(&owner).is_some_and(|owner| owner == id);
        self.save(&all).map_err(|e| e.to_string())?;
        Ok(owns)
    }

    /// Move the pre-selector token into its original server's slot before
    /// background tasks start. It can never become another server's token.
    pub fn migrate_account_token(&self) -> Result<(), String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all = self.load()?;
        let Some(api) = self.legacy_api.as_deref() else {
            return Ok(());
        };
        let Some(token) = all.remove(uxduck_engine::ACCOUNT_TOKEN) else {
            return Ok(());
        };
        all.entry(account_key(api)).or_insert(token);
        self.save(&all)
            .map_err(|e| format!("cannot migrate {}: {e}", self.path.display()))
    }

    /// Writes the store to a new file of its own with a name no one else
    /// chose, readable only by this account from the moment it exists, and
    /// then puts it in place of the old one in one step (FB-268).
    fn save(&self, all: &BTreeMap<String, String>) -> std::io::Result<()> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut temp = tempfile::Builder::new()
            .prefix("credentials.json.")
            .suffix(".tmp")
            .make_in(dir, create_private)?;
        std::io::Write::write_all(
            temp.as_file_mut(),
            &serde_json::to_vec_pretty(all).unwrap_or_default(),
        )?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

impl Credentials for FileCredentials {
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all = self.load()?;
        let scoped;
        let name = if name == uxduck_engine::ACCOUNT_TOKEN {
            let api = self
                .api
                .as_deref()
                .ok_or("This UXDuck has no account server.")?;
            // A new credential is unverified until its own /v1/me completes.
            all.remove(&self.scoped("account.person"));
            scoped = account_key(api);
            if self.api == self.legacy_api {
                all.remove(name);
            }
            &scoped
        } else {
            name
        };
        if value.is_empty() {
            all.remove(name);
        } else {
            all.insert(name.to_owned(), value.to_owned());
        }
        self.save(&all)
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

/// Makes a new file at `path` that only the account UXDuck runs as may
/// open: `0600`, or on Windows permissions of its own and none from its
/// folder, given as the file is made. A file already at `path` is never
/// opened: making it fails. For the credential store and for the keys and
/// passwords `ssh` is given (FB-268), which Windows' OpenSSH refuses when
/// any other account may open them.
pub(crate) fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        create_private_windows(path)
    }
    #[cfg(not(windows))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(path)
    }
}

/// Makes the folder `path` when it is missing, with its parents, so that
/// only the account UXDuck runs as may open or change it: `0700`, or on
/// Windows a protected list granting this account alone, which what is
/// made in it inherits. A folder already there is left as it is. For the
/// companions folder, where `ssh` is given keys and passwords (FB-268).
pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(windows)]
    let made = create_private_dir_windows(path);
    #[cfg(not(windows))]
    let made = {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(path)
    };
    match made {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        made => made,
    }
}

/// Why `path` is not a folder only this account may open or change, when
/// it is not: it must be a folder itself, not a link, owned by this
/// account and `0700`, or on Windows owned by this account with a protected
/// list granting this account alone. What is in such a folder cannot be
/// changed by another account between looking at it and removing it.
pub(crate) fn private_dir_problem(path: &Path) -> Option<String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) => return Some(format!("it cannot be read: {error}")),
    };
    if meta.file_type().is_symlink() {
        return Some("it is a link".to_owned());
    }
    if !meta.is_dir() {
        return Some("it is not a folder".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.uid() != rustix::process::geteuid().as_raw() {
            return Some("another account owns it".to_owned());
        }
        let mode = meta.mode() & 0o777;
        if mode != 0o700 {
            return Some(format!("other accounts may open it: its mode is {mode:o}"));
        }
        None
    }
    #[cfg(windows)]
    {
        // FILE_ATTRIBUTE_REPARSE_POINT: a junction or another link.
        if std::os::windows::fs::MetadataExt::file_attributes(&meta) & 0x400 != 0 {
            return Some("it is a junction or another link".to_owned());
        }
        private_dir_problem_windows(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Some("UXDuck cannot check who may open folders on this system".to_owned())
    }
}

/// [`create_private`] on Windows: `CreateFileW` with `CREATE_NEW` and a
/// protected list granting full control to this process's account alone.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "making a file with permissions is a plain Win32 call"
)]
fn create_private_windows(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    // Full control for this account alone, and none inherited from the folder.
    let attributes = private_attributes_windows("")?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a path ending in a zero and the descriptor made above, which
    // stays valid until it is freed once the file is made. Others may read
    // and replace the file while it is open, as the credential store's
    // rename needs, but only this account may open it.
    let (file, error) = unsafe {
        let file = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        let error = std::io::Error::last_os_error();
        LocalFree(attributes.lpSecurityDescriptor);
        (file, error)
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: a handle CreateFileW just opened, owned by the File from now on.
    Ok(unsafe { std::fs::File::from_raw_handle(file) })
}

/// [`create_private_dir`] on Windows: `CreateDirectoryW` with a protected
/// list granting full control to this process's account alone, which what
/// is made in the folder inherits.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "making a folder with permissions is a plain Win32 call"
)]
fn create_private_dir_windows(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let attributes = private_attributes_windows("OICI")?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a path ending in a zero and the descriptor made above, freed
    // once the folder is made.
    let (made, error) = unsafe {
        let made = CreateDirectoryW(wide.as_ptr(), &raw const attributes);
        let error = std::io::Error::last_os_error();
        LocalFree(attributes.lpSecurityDescriptor);
        (made, error)
    };
    if made == 0 {
        return Err(error);
    }
    Ok(())
}

/// The account this process runs as: its token's `TOKEN_USER` record, whose
/// SID lives in the same buffer.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "reading this process's account is a plain Win32 call"
)]
fn account_windows() -> std::io::Result<Vec<u64>> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle of this process, and a handle to fill in.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut size = 0u32;
    // SAFETY: asks only for the size of the account's record.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut size) };
    let mut record = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: `record` holds `size` bytes, aligned for a TOKEN_USER.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            record.as_mut_ptr().cast(),
            size,
            &raw mut size,
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: the token opened above, closed once.
    unsafe { CloseHandle(token) };
    if read == 0 {
        return Err(error);
    }
    Ok(record)
}

/// Security attributes owned by this process's account and granting it
/// alone full control, with the ACE flags `inherit` (`OICI` for a folder
/// whose contents inherit it). Free `lpSecurityDescriptor` with `LocalFree`.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "a security descriptor from SDDL is a plain Win32 call"
)]
fn private_attributes_windows(
    inherit: &str,
) -> std::io::Result<windows_sys::Win32::Security::SECURITY_ATTRIBUTES> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_USER};
    let record = account_windows()?;
    // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of `record`.
    let account = unsafe { &*record.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: a valid SID from the token, and a string for Windows to allocate.
    if unsafe { ConvertSidToStringSidW(account, &raw mut text) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Windows wrote a string ending in a zero at `text`, freed once.
    let sid = unsafe {
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        sid
    };
    let sddl: Vec<u16> = format!("O:{sid}D:P(A;{inherit};FA;;;{sid})")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a string ending in a zero, and a descriptor for Windows to allocate.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    })
}

/// [`private_dir_problem`] on Windows: the folder, opened without following
/// a link, is owned by this process's account, its list does not take
/// entries from its parent, and it holds one entry, allowing this account.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "reading a folder's owner and permissions is a plain Win32 call"
)]
fn private_dir_problem_windows(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SE_DACL_PROTECTED, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
    };
    let record = match account_windows() {
        Ok(record) => record,
        Err(error) => return Some(format!("UXDuck cannot read its own account: {error}")),
    };
    // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of `record`.
    let account = unsafe { &*record.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a path ending in a zero. The folder itself is opened, never
    // what a link leads to, only to read its permissions.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Some(format!(
            "it cannot be opened: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut owner: PSID = std::ptr::null_mut();
    let mut list: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a handle opened above with READ_CONTROL, closed once, and
    // places for Windows to point into the descriptor it allocates.
    let read = unsafe {
        let read = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut list,
            std::ptr::null_mut(),
            &raw mut descriptor,
        );
        CloseHandle(handle);
        read
    };
    if read != 0 {
        return Some(format!(
            "its permissions cannot be read: {}",
            std::io::Error::from_raw_os_error(i32::try_from(read).unwrap_or(i32::MAX))
        ));
    }
    // SAFETY: the owner and list point into `descriptor`, valid until it is
    // freed once below, and the one entry read is an ACCESS_ALLOWED_ACE
    // when its type is 0 (ACCESS_ALLOWED_ACE_TYPE).
    let why = unsafe {
        'check: {
            if EqualSid(owner, account) == 0 {
                break 'check Some("another account owns it");
            }
            let mut control = 0u16;
            let mut revision = 0u32;
            if GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) == 0
                || control & SE_DACL_PROTECTED == 0
            {
                break 'check Some("it takes permissions from its parent folder");
            }
            if list.is_null() || (*list).AceCount != 1 {
                break 'check Some("other accounts may open it");
            }
            let mut entry: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(list, 0, &raw mut entry) == 0 {
                break 'check Some("its permissions cannot be read");
            }
            let entry = &*entry.cast::<ACCESS_ALLOWED_ACE>();
            if entry.Header.AceType != 0
                || EqualSid((&raw const entry.SidStart).cast_mut().cast(), account) == 0
            {
                break 'check Some("other accounts may open it");
            }
            None
        }
    };
    // SAFETY: the descriptor GetSecurityInfo allocated, freed once.
    unsafe { LocalFree(descriptor) };
    why.map(str::to_owned)
}

fn fingerprint(token: &str) -> String {
    format!(
        "{:x}",
        uxduck_core::encoding::Hex(&Sha256::digest(token.as_bytes()))
    )
}

fn account_key(api: &str) -> String {
    format!("account.token:{}", api.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_persons_login_never_becomes_a_shared_data_export_credential() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileCredentials::new(dir.path());
        store.api = Some("https://api.uxduck.test".into());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "ana-token")
            .unwrap();
        assert!(
            store.get(uxduck_engine::ACCOUNT_TOKEN).is_none(),
            "unverified tokens cannot export shared data"
        );
        assert!(store.remember_person("ana-token", "ana").unwrap());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "ben-token")
            .unwrap();
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        assert!(!store.remember_person("ben-token", "ben").unwrap());
        assert_eq!(store.account_session().as_deref(), Some("ben-token"));
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "ana-new-token")
            .unwrap();
        assert!(store.remember_person("ana-new-token", "ana").unwrap());
        assert_eq!(
            store.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("ana-new-token")
        );
    }

    #[test]
    fn legacy_ownership_and_corrupt_credentials_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileCredentials::new(dir.path());
        store.api = Some("https://api.uxduck.test".into());
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "legacy-token")
            .unwrap();
        store.protect_legacy_workspace().unwrap();
        store
            .set(uxduck_engine::ACCOUNT_TOKEN, "another-person")
            .unwrap();
        assert!(!store.remember_person("another-person", "ben").unwrap());
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        std::fs::write(&store.path, "broken").unwrap();
        assert!(store.get(uxduck_engine::ACCOUNT_TOKEN).is_none());
        assert!(
            store
                .set(uxduck_engine::ACCOUNT_TOKEN, "cannot-overwrite")
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "broken");
    }

    #[test]
    fn stores_reads_and_removes_privately() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileCredentials::new(dir.path());
        assert!(!store.has("openai.key"));
        store.set("openai.key", "sk-1").unwrap();
        assert_eq!(
            FileCredentials::new(dir.path())
                .get("openai.key")
                .as_deref(),
            Some("sk-1")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("credentials.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.set("openai.key", "").unwrap();
        assert!(!store.has("openai.key"));
    }

    /// The store is written to a new file of its own before it replaces
    /// the old one, so a file someone left at a name UXDuck used to write
    /// first is never written through (FB-268).
    #[cfg(unix)]
    #[test]
    fn the_store_is_written_to_a_new_private_file_and_nothing_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "untouched").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("credentials.json.tmp")).unwrap();
        let mut store = FileCredentials::new(dir.path());
        store.set("openai.key", "sk-1").unwrap();
        store.set("openai.key", "sk-2").unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "untouched");
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["credentials.json", "credentials.json.tmp", "elsewhere"]
        );
        let mode = std::fs::metadata(dir.path().join("credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(create_private(&dir.path().join("credentials.json")).is_err());
    }

    #[test]
    fn account_tokens_are_isolated_and_sign_out_only_removes_the_active_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut staging = FileCredentials::new(dir.path());
        staging.api = Some("https://api-staging.uxduck.com".into());
        let mut production = staging.clone();
        production.api = Some("https://api.uxduck.com".into());
        staging
            .set(uxduck_engine::ACCOUNT_TOKEN, "staging-token")
            .unwrap();
        staging
            .remember_person("staging-token", "staging-person")
            .unwrap();
        assert!(!production.has(uxduck_engine::ACCOUNT_TOKEN));
        production
            .set(uxduck_engine::ACCOUNT_TOKEN, "production-token")
            .unwrap();
        production
            .remember_person("production-token", "production-person")
            .unwrap();
        assert_eq!(
            staging.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("staging-token")
        );
        assert_eq!(
            production.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("production-token")
        );
        production.set(uxduck_engine::ACCOUNT_TOKEN, "").unwrap();
        assert!(!production.has(uxduck_engine::ACCOUNT_TOKEN));
        assert!(staging.has(uxduck_engine::ACCOUNT_TOKEN));
    }

    #[test]
    fn legacy_tokens_are_migrated_to_the_startup_server_even_with_a_pending_choice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("credentials.json"),
            r#"{"account.token":"old","openai.key":"key"}"#,
        )
        .unwrap();
        let mut credentials = FileCredentials::new(dir.path());
        credentials.legacy_api = Some("https://api-staging.uxduck.com".into());
        credentials.api = Some("https://api.uxduck.com".into());
        credentials.migrate_account_token().unwrap();
        assert!(!credentials.has(uxduck_engine::ACCOUNT_TOKEN));
        assert_eq!(credentials.get("openai.key").as_deref(), Some("key"));
        credentials.api = credentials.legacy_api.clone();
        credentials.protect_legacy_workspace().unwrap();
        assert_eq!(
            credentials.get(uxduck_engine::ACCOUNT_TOKEN).as_deref(),
            Some("old")
        );
        assert!(
            !credentials
                .load()
                .unwrap()
                .contains_key(uxduck_engine::ACCOUNT_TOKEN)
        );
    }
}
