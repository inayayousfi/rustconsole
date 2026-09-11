use crate::credentials::{DATA_DIRECTORY, OPAQUE_RECORD_PATH};
use crate::firewall::FirewallScope;
use crate::service::{SERVICE_ERROR_REPORT, SERVICE_NAME, install_service};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const INSTALL_DIRECTORY: &str = r"C:\Program Files\RustConsole";
const DRIVER_RECORD_PATH: &str = r"C:\ProgramData\RustConsole\installed-driver.txt";
const LEGACY_SERVICE_NAME: &str = "RustConsoleHostDev";
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

pub fn install(elevated: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !elevated {
        return elevate("install-elevated");
    }
    require_elevated()?;

    let package = std::env::current_exe()?
        .parent()
        .ok_or("the package executable has no parent directory")?
        .to_owned();
    let packaged_host = package.join("rustconsole-host.exe");
    let packaged_driver = package.join("input-driver");
    let driver_inf = packaged_driver.join("rustconsole_input_driver.inf");
    let driver_certificate = packaged_driver.join("RustConsoleLocalDriverSigning.cer");
    for path in [&packaged_host, &driver_inf, &driver_certificate] {
        if !path.is_file() {
            return Err(format!(
                "the Rust Console host package is missing {}",
                path.display()
            )
            .into());
        }
    }

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    if service_exists(&manager, LEGACY_SERVICE_NAME)? {
        return Err(format!(
            "legacy service {LEGACY_SERVICE_NAME} is installed; remove it manually before using the new installer"
        )
        .into());
    }
    stop_and_delete_service(&manager, SERVICE_NAME)?;

    run_checked(
        "trusting the driver certificate in LocalMachine\\Root",
        Command::new("certutil.exe")
            .args(["-addstore", "-f", "Root"])
            .arg(&driver_certificate),
    )?;
    run_checked(
        "trusting the driver certificate in LocalMachine\\TrustedPublisher",
        Command::new("certutil.exe")
            .args(["-addstore", "-f", "TrustedPublisher"])
            .arg(&driver_certificate),
    )?;

    let previous_driver = recorded_driver_name()?;
    let output = run_checked(
        "staging the Rust Console input driver",
        Command::new("pnputil.exe")
            .args(["/add-driver"])
            .arg(&driver_inf),
    )?;
    let published_name = published_driver_name(&output)
        .ok_or("pnputil did not report the published OEM INF name")?;
    let installation = PathBuf::from(INSTALL_DIRECTORY);
    if installation.exists() {
        fs::remove_dir_all(&installation)?;
    }
    copy_directory(&package, &installation)?;
    let installed_host = installation.join("rustconsole-host.exe");
    install_service(installed_host.clone(), Vec::new())?;
    crate::firewall::enable(FirewallScope::AllProfilesLocalSubnet)?;
    if !Path::new(OPAQUE_RECORD_PATH).is_file() {
        crate::credentials::set_password_interactive()?;
    }
    remove_file_if_present(Path::new(SERVICE_ERROR_REPORT))?;
    start_service(&manager)?;
    if let Some(previous_driver) = previous_driver
        && previous_driver != published_name
    {
        remove_driver_package(&previous_driver, true)?;
    }
    fs::create_dir_all(DATA_DIRECTORY)?;
    fs::write(DRIVER_RECORD_PATH, format!("{published_name}\n"))?;
    println!("Rust Console Host is installed and running.");
    Ok(())
}

pub fn uninstall(elevated: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !elevated {
        return elevate("uninstall-elevated");
    }
    require_elevated()?;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    rustconsole_input_windows::remove_persistent_devices()?;
    stop_and_delete_service(&manager, SERVICE_NAME)?;
    crate::firewall::disable()?;
    remove_recorded_driver()?;
    remove_certificate("Root")?;
    remove_certificate("TrustedPublisher")?;

    let installation = Path::new(INSTALL_DIRECTORY);
    if installation.exists() {
        fs::remove_dir_all(installation)?;
    }
    remove_data_except_credentials()?;
    println!("Rust Console Host is uninstalled; opaque-record.bin was preserved.");
    Ok(())
}

fn elevate(command: &str) -> Result<(), Box<dyn std::error::Error>> {
    let executable = wide(std::env::current_exe()?.as_os_str());
    let parameters = wide(OsStr::new(command));
    let verb = wide(OsStr::new("runas"));
    let mut execute = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>())?,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: execute and all referenced strings remain live for the call.
    unsafe { ShellExecuteExW(&mut execute)? };
    if execute.hProcess.is_invalid() {
        return Err("UAC did not return an installer process handle".into());
    }
    // SAFETY: ShellExecuteExW returned an owned process handle.
    let wait = unsafe { WaitForSingleObject(execute.hProcess, INFINITE) };
    if wait != WAIT_OBJECT_0 {
        let _ = unsafe { CloseHandle(execute.hProcess) };
        return Err("waiting for the elevated installer failed".into());
    }
    let mut exit_code = 1;
    // SAFETY: the process has exited and exit_code is valid output storage.
    unsafe { GetExitCodeProcess(execute.hProcess, &mut exit_code)? };
    let _ = unsafe { CloseHandle(execute.hProcess) };
    if exit_code != 0 {
        return Err(format!("the elevated installer exited with code {exit_code}").into());
    }
    Ok(())
}

fn require_elevated() -> Result<(), Box<dyn std::error::Error>> {
    let mut token = HANDLE::default();
    // SAFETY: token is valid output storage.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
    let result = (|| {
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        // SAFETY: token and elevation storage are valid.
        unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>())?,
                &mut returned,
            )?;
        }
        if elevation.TokenIsElevated == 0 {
            return Err("installation requires administrator elevation".into());
        }
        Ok(())
    })();
    let _ = unsafe { CloseHandle(token) };
    result
}

fn service_exists(
    manager: &ServiceManager,
    name: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    match manager.open_service(name, ServiceAccess::QUERY_STATUS) {
        Ok(service) => {
            drop(service);
            Ok(true)
        }
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

fn stop_and_delete_service(
    manager: &ServiceManager,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = match manager.open_service(
        name,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
        wait_for_state(&service, ServiceState::Stopped, SERVICE_TIMEOUT)?;
    }
    service.delete()?;
    drop(service);

    let started = Instant::now();
    while started.elapsed() < DELETE_TIMEOUT {
        if !service_exists(manager, name)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out waiting for service {name} to be deleted").into())
}

fn start_service(manager: &ServiceManager) -> Result<(), Box<dyn std::error::Error>> {
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::START,
    )?;
    service.start::<OsString>(&[])?;
    wait_for_state(&service, ServiceState::Running, SERVICE_TIMEOUT)
}

fn wait_for_state(
    service: &windows_service::service::Service,
    expected: ServiceState,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if service.query_status()?.current_state == expected {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out waiting for service state {expected:?}").into())
}

fn run_checked(action: &str, command: &mut Command) -> Result<Output, Box<dyn std::error::Error>> {
    let output = command.output()?;
    print_output(&output);
    if !output.status.success() {
        return Err(format!("{action} failed with {}", output.status).into());
    }
    Ok(output)
}

fn print_output(output: &Output) {
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
}

fn published_driver_name(output: &Output) -> Option<String> {
    published_driver_name_bytes(&output.stdout)
}

fn published_driver_name_bytes(stdout: &[u8]) -> Option<String> {
    stdout
        .split(|byte| byte.is_ascii_whitespace())
        .filter_map(|word| std::str::from_utf8(word).ok())
        .map(|word| {
            word.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.'
            })
        })
        .find(|word| {
            let lower = word.to_ascii_lowercase();
            lower.starts_with("oem")
                && lower.ends_with(".inf")
                && lower[3..lower.len() - 4]
                    .chars()
                    .all(|character| character.is_ascii_digit())
        })
        .map(str::to_ascii_lowercase)
}

fn remove_recorded_driver() -> Result<(), Box<dyn std::error::Error>> {
    let Some(published_name) = recorded_driver_name()? else {
        return Ok(());
    };
    remove_driver_package(&published_name, true)?;
    fs::remove_file(DRIVER_RECORD_PATH)?;
    Ok(())
}

fn recorded_driver_name() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let record = Path::new(DRIVER_RECORD_PATH);
    if !record.is_file() {
        return Ok(None);
    }
    let published_name = fs::read_to_string(record)?;
    let published_name = published_name.trim().to_ascii_lowercase();
    if !is_published_driver_name(&published_name) {
        return Err("the recorded driver package name is invalid".into());
    }
    Ok(Some(published_name))
}

fn remove_driver_package(
    published_name: &str,
    uninstall: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("pnputil.exe");
    command.args(["/delete-driver", published_name]);
    if uninstall {
        command.arg("/uninstall");
    }
    command.arg("/force");
    run_checked(
        "removing the previous Rust Console input driver",
        &mut command,
    )?;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_published_driver_name(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("oem")
        && lower.ends_with(".inf")
        && lower.len() > 7
        && lower[3..lower.len() - 4]
            .chars()
            .all(|character| character.is_ascii_digit())
}

fn remove_certificate(store: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_checked(
        &format!("removing RustConsoleLocalDriverSigning from LocalMachine\\{store}"),
        Command::new("certutil.exe").args(["-delstore", store, "RustConsoleLocalDriverSigning"]),
    )?;
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn remove_data_except_credentials() -> io::Result<()> {
    let directory = Path::new(DATA_DIRECTORY);
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.path() == Path::new(OPAQUE_RECORD_PATH) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_driver_name_accepts_only_bounded_oem_inf_shape() {
        assert!(is_published_driver_name("oem579.inf"));
        assert!(is_published_driver_name("OEM1.INF"));
        assert!(!is_published_driver_name("rustconsole_input_driver.inf"));
        assert!(!is_published_driver_name("oem.inf"));
        assert!(!is_published_driver_name("oem12.inf.exe"));
    }

    #[test]
    fn published_driver_name_is_found_without_parsing_localized_labels() {
        assert_eq!(
            published_driver_name_bytes(b"Nom publie : oem579.inf\r\n").as_deref(),
            Some("oem579.inf")
        );
    }
}
