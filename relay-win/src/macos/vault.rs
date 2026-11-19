use std::fs::{File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::OnceLock;

use security_framework::passwords::{delete_generic_password, generic_password, set_generic_password, PasswordOptions};

const SERVICE: &str = "io.github.ducvd89.Russenger";
const ACCOUNT: &str = "Facebook session";
static INSTANCE: OnceLock<File> = OnceLock::new();

pub fn data_dir() -> PathBuf {
    let path = crate::paths::data_dir();
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&path);
    path
}

pub fn save_cookies(cookies: &str) -> bool {
    set_generic_password(SERVICE, ACCOUNT, cookies.as_bytes()).is_ok()
}

pub fn load_cookies() -> Option<String> {
    let options = PasswordOptions::new_generic_password(SERVICE, ACCOUNT);
    String::from_utf8(generic_password(options).ok()?).ok()
}

pub fn clear_cookies() {
    let _ = delete_generic_password(SERVICE, ACCOUNT);
}

pub fn acquire_single_instance() -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(data_dir().join("instance.lock"))
    else {
        return false;
    };
    file.try_lock().is_ok() && INSTANCE.set(file).is_ok()
}
