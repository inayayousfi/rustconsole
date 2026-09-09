use rustconsole_host_core::authentication::OpaqueServerRecord;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;
use zeroize::Zeroizing;

pub const DATA_DIRECTORY: &str = r"C:\ProgramData\RustConsole";
pub const OPAQUE_RECORD_PATH: &str = r"C:\ProgramData\RustConsole\opaque-record.bin";

const RESTRICTED_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

pub fn set_password_interactive() -> Result<(), Box<dyn std::error::Error>> {
    require_elevated_administrator()?;
    let password = Zeroizing::new(rpassword::prompt_password("New Rust Console password: ")?);
    let confirmation = Zeroizing::new(rpassword::prompt_password("Confirm password: ")?);
    set_password(&password, &confirmation)
}

pub fn set_password_from_stdin() -> Result<(), Box<dyn std::error::Error>> {
    require_elevated_administrator()?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let password = read_secret_line(&mut input, "password")?;
    let confirmation = read_secret_line(&mut input, "password confirmation")?;
    set_password(&password, &confirmation)
}

fn set_password(password: &str, confirmation: &str) -> Result<(), Box<dyn std::error::Error>> {
    validate_password_confirmation(password, confirmation)?;

    let record = match load_record() {
        Ok(existing) => existing.replace_password(password.as_bytes().to_vec())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            OpaqueServerRecord::enroll(password.as_bytes().to_vec())?
        }
        Err(error) => return Err(error.into()),
    };
    store_record(&record)?;
    println!("Rust Console password record replaced.");
    Ok(())
}

fn read_secret_line<R: BufRead>(input: &mut R, name: &str) -> io::Result<Zeroizing<String>> {
    let mut value = Zeroizing::new(String::new());
    if input.read_line(&mut value)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("missing {name}"),
        ));
    }
    while value.ends_with(['\r', '\n']) {
        value.pop();
    }
    Ok(value)
}

fn validate_password_confirmation(password: &str, confirmation: &str) -> io::Result<()> {
    if password != confirmation {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "password confirmation does not match",
        ));
    }
    if password.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "password must not be empty",
        ));
    }
    Ok(())
}

pub fn load_record() -> io::Result<OpaqueServerRecord> {
    let bytes = fs::read(OPAQUE_RECORD_PATH)?;
    OpaqueServerRecord::deserialize(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn store_record(record: &OpaqueServerRecord) -> io::Result<()> {
    store_restricted_file(Path::new(OPAQUE_RECORD_PATH), &record.serialize())
}

pub(crate) fn store_restricted_file(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let descriptor = LocalSecurityDescriptor::restricted()?;
    create_restricted_directory(Path::new(DATA_DIRECTORY), &descriptor)?;
    let temporary = temporary_path(target);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        apply_restricted_descriptor(&temporary, &descriptor)?;
        drop(file);

        let temporary_wide = wide_path(&temporary);
        let target_wide = wide_path(target);
        unsafe {
            MoveFileExW(
                PCWSTR(temporary_wide.as_ptr()),
                PCWSTR(target_wide.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
            .map_err(io::Error::from)?;
        }
        apply_restricted_descriptor(target, &descriptor)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_restricted_directory(
    path: &Path,
    descriptor: &LocalSecurityDescriptor,
) -> io::Result<()> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    let path_wide = wide_path(path);
    match unsafe { CreateDirectoryW(PCWSTR(path_wide.as_ptr()), Some(&attributes)) } {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_ALREADY_EXISTS.to_hresult() && path.is_dir() => {}
        Err(error) => return Err(io::Error::from(error)),
    }
    apply_restricted_descriptor(path, descriptor)
}

fn apply_restricted_descriptor(
    path: &Path,
    descriptor: &LocalSecurityDescriptor,
) -> io::Result<()> {
    let path_wide = wide_path(path);
    unsafe {
        windows::Win32::Security::SetFileSecurityW(
            PCWSTR(path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor.0,
        )
        .ok()
        .map_err(io::Error::from)
    }
}

fn temporary_path(target: &Path) -> PathBuf {
    target.with_extension(format!("tmp-{}", std::process::id()))
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl LocalSecurityDescriptor {
    fn restricted() -> io::Result<Self> {
        let sddl = RESTRICTED_SDDL
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .map_err(io::Error::from)?;
        }
        Ok(Self(descriptor))
    }
}

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

fn require_elevated_administrator() -> io::Result<()> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(io::Error::from)?;
    }
    let result = (|| {
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
            .map_err(io::Error::from)?;
        }
        if returned != size_of::<TOKEN_ELEVATION>() as u32 || elevation.TokenIsElevated == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "set-password requires an elevated administrator terminal",
            ));
        }
        Ok(())
    })();
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn stdin_password_reads_two_lines_without_line_endings() {
        let mut input = Cursor::new(b"secret\r\nsecret\n");

        let password = read_secret_line(&mut input, "password").unwrap();
        let confirmation = read_secret_line(&mut input, "password confirmation").unwrap();

        assert_eq!(&*password, "secret");
        assert_eq!(&*confirmation, "secret");
    }

    #[test]
    fn password_validation_rejects_empty_and_mismatched_input() {
        assert_eq!(
            validate_password_confirmation("", "").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_password_confirmation("first", "second")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
