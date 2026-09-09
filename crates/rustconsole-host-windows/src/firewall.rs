use rustconsole_protocol::wire::HostFirewallStatus;
use std::fmt;

pub const FIREWALL_REPORT_PATH: &str = r"C:\ProgramData\RustConsole\firewall-status.txt";
pub const FIREWALL_PORT: u16 = 47_999;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirewallScope {
    PrivateLocalSubnet,
    AllProfilesLocalSubnet,
    AllAddresses,
}

impl FirewallScope {
    pub const fn status(self) -> HostFirewallStatus {
        match self {
            Self::PrivateLocalSubnet => HostFirewallStatus::PrivateLocalSubnet,
            Self::AllProfilesLocalSubnet => HostFirewallStatus::AllProfilesLocalSubnet,
            Self::AllAddresses => HostFirewallStatus::AllAddresses,
        }
    }

    pub const fn argument(self) -> &'static str {
        match self {
            Self::PrivateLocalSubnet => "private-local-subnet",
            Self::AllProfilesLocalSubnet => "all-profiles-local-subnet",
            Self::AllAddresses => "all-addresses",
        }
    }

    #[cfg(windows)]
    const fn rule_name(self) -> &'static str {
        match self {
            Self::PrivateLocalSubnet => "Rust Console UDP 47999 - Private local subnet",
            Self::AllProfilesLocalSubnet => "Rust Console UDP 47999 - All profiles local subnet",
            Self::AllAddresses => "Rust Console UDP 47999 - All addresses",
        }
    }

    #[cfg(windows)]
    const fn remote_addresses(self) -> &'static str {
        match self {
            Self::PrivateLocalSubnet | Self::AllProfilesLocalSubnet => "LocalSubnet",
            Self::AllAddresses => "*",
        }
    }

    const ALL: [Self; 3] = [
        Self::PrivateLocalSubnet,
        Self::AllProfilesLocalSubnet,
        Self::AllAddresses,
    ];
}

impl fmt::Display for FirewallScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.argument())
    }
}

pub fn guidance(status: HostFirewallStatus) -> String {
    let mut report = format!("Rust Console firewall status: {}\n", status_name(status));
    if matches!(
        status,
        HostFirewallStatus::Missing | HostFirewallStatus::CheckFailed
    ) {
        report.push_str("Choose one command in an elevated terminal:\n");
        for scope in FirewallScope::ALL {
            report.push_str(&format!(
                "  rustconsole-host firewall enable {}\n",
                scope.argument()
            ));
        }
    }
    report.push_str("Inspect with: rustconsole-host firewall status\n");
    report.push_str("Remove with: rustconsole-host firewall disable\n");
    report
}

pub const fn status_name(status: HostFirewallStatus) -> &'static str {
    match status {
        HostFirewallStatus::Unknown => "unknown",
        HostFirewallStatus::Missing => "missing",
        HostFirewallStatus::PrivateLocalSubnet => "private-local-subnet",
        HostFirewallStatus::AllProfilesLocalSubnet => "all-profiles-local-subnet",
        HostFirewallStatus::AllAddresses => "all-addresses",
        HostFirewallStatus::CheckFailed => "check-failed",
    }
}

#[cfg(windows)]
pub fn status() -> HostFirewallStatus {
    checked_status().unwrap_or(HostFirewallStatus::CheckFailed)
}

#[cfg(not(windows))]
pub fn status() -> HostFirewallStatus {
    HostFirewallStatus::Unknown
}

#[cfg(windows)]
pub fn print_status() -> Result<(), Box<dyn std::error::Error>> {
    let status = checked_status()?;
    print!("{}", guidance(status));
    Ok(())
}

#[cfg(not(windows))]
pub fn print_status() -> Result<(), Box<dyn std::error::Error>> {
    Err("Windows Firewall is only available on Windows".into())
}

#[cfg(windows)]
pub fn write_report() -> std::io::Result<HostFirewallStatus> {
    let status = status();
    let path = std::path::Path::new(FIREWALL_REPORT_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, guidance(status))?;
    Ok(status)
}

#[cfg(windows)]
pub fn enable(scope: FirewallScope) -> Result<(), Box<dyn std::error::Error>> {
    let (_com, rules) = open_rules()?;
    let created = if let Some(rule) = find_rule(&rules, scope)? {
        validate_rule(&rule, scope)?;
        false
    } else {
        add_rule(&rules, scope)?;
        validate_rule(
            &find_rule(&rules, scope)?.ok_or("Windows Firewall did not retain the new rule")?,
            scope,
        )?;
        true
    };

    let mut removed = Vec::new();
    for other in FirewallScope::ALL {
        if other == scope || find_rule(&rules, other)?.is_none() {
            continue;
        }
        if let Err(error) = unsafe { rules.Remove(&windows::core::BSTR::from(other.rule_name())) } {
            for previous in removed {
                let _ = add_rule(&rules, previous);
            }
            if created {
                let _ = unsafe { rules.Remove(&windows::core::BSTR::from(scope.rule_name())) };
            }
            return Err(error.into());
        }
        removed.push(other);
    }
    write_report()?;
    print_status()
}

#[cfg(not(windows))]
pub fn enable(_scope: FirewallScope) -> Result<(), Box<dyn std::error::Error>> {
    Err("Windows Firewall is only available on Windows".into())
}

#[cfg(windows)]
pub fn disable() -> Result<(), Box<dyn std::error::Error>> {
    let (_com, rules) = open_rules()?;
    for scope in FirewallScope::ALL {
        if find_rule(&rules, scope)?.is_some() {
            unsafe { rules.Remove(&windows::core::BSTR::from(scope.rule_name()))? };
        }
    }
    write_report()?;
    print_status()
}

#[cfg(not(windows))]
pub fn disable() -> Result<(), Box<dyn std::error::Error>> {
    Err("Windows Firewall is only available on Windows".into())
}

#[cfg(windows)]
fn add_rule(
    rules: &windows::Win32::NetworkManagement::WindowsFirewall::INetFwRules,
    scope: FirewallScope,
) -> windows::core::Result<()> {
    use windows::Win32::Foundation::VARIANT_TRUE;
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE2_ALL,
        NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_RULE_DIR_IN, NetFwRule,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::core::BSTR;

    unsafe {
        let rule: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
        rule.SetName(&BSTR::from(scope.rule_name()))?;
        rule.SetDescription(&BSTR::from(
            "Allows authenticated Rust Console QUIC sessions on UDP 47999.",
        ))?;
        rule.SetProtocol(NET_FW_IP_PROTOCOL_UDP.0)?;
        rule.SetLocalPorts(&BSTR::from(FIREWALL_PORT.to_string()))?;
        rule.SetRemoteAddresses(&BSTR::from(scope.remote_addresses()))?;
        rule.SetDirection(NET_FW_RULE_DIR_IN)?;
        rule.SetProfiles(match scope {
            FirewallScope::PrivateLocalSubnet => {
                NET_FW_PROFILE2_DOMAIN.0 | NET_FW_PROFILE2_PRIVATE.0
            }
            FirewallScope::AllProfilesLocalSubnet | FirewallScope::AllAddresses => {
                NET_FW_PROFILE2_ALL.0
            }
        })?;
        rule.SetAction(NET_FW_ACTION_ALLOW)?;
        rule.SetEnabled(VARIANT_TRUE)?;
        rules.Add(&rule)
    }
}

#[cfg(windows)]
fn checked_status() -> windows::core::Result<HostFirewallStatus> {
    let (_com, rules) = open_rules()?;
    let mut found = None;
    for scope in FirewallScope::ALL {
        let Some(rule) = find_rule(&rules, scope)? else {
            continue;
        };
        if found.is_some() || validate_rule(&rule, scope).is_err() {
            return Ok(HostFirewallStatus::CheckFailed);
        }
        found = Some(scope.status());
    }
    Ok(found.unwrap_or(HostFirewallStatus::Missing))
}

#[cfg(windows)]
fn validate_rule(
    rule: &windows::Win32::NetworkManagement::WindowsFirewall::INetFwRule,
    scope: FirewallScope,
) -> windows::core::Result<()> {
    use windows::Win32::Foundation::VARIANT_FALSE;
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        NET_FW_ACTION_ALLOW, NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE2_ALL, NET_FW_PROFILE2_DOMAIN,
        NET_FW_PROFILE2_PRIVATE, NET_FW_RULE_DIR_IN,
    };
    use windows::core::{Error, HRESULT};

    let expected_profiles = match scope {
        FirewallScope::PrivateLocalSubnet => NET_FW_PROFILE2_DOMAIN.0 | NET_FW_PROFILE2_PRIVATE.0,
        FirewallScope::AllProfilesLocalSubnet | FirewallScope::AllAddresses => {
            NET_FW_PROFILE2_ALL.0
        }
    };
    let valid = unsafe {
        rule.Enabled()? != VARIANT_FALSE
            && rule.Direction()? == NET_FW_RULE_DIR_IN
            && rule.Action()? == NET_FW_ACTION_ALLOW
            && rule.Protocol()? == NET_FW_IP_PROTOCOL_UDP.0
            && rule.LocalPorts()?.to_string() == FIREWALL_PORT.to_string()
            && rule.Profiles()? == expected_profiles
            && rule.RemoteAddresses()?.to_string() == scope.remote_addresses()
    };
    if valid {
        Ok(())
    } else {
        Err(Error::from_hresult(HRESULT(0x8007_000D_u32 as i32)))
    }
}

#[cfg(windows)]
fn find_rule(
    rules: &windows::Win32::NetworkManagement::WindowsFirewall::INetFwRules,
    scope: FirewallScope,
) -> windows::core::Result<Option<windows::Win32::NetworkManagement::WindowsFirewall::INetFwRule>> {
    match unsafe { rules.Item(&windows::core::BSTR::from(scope.rule_name())) } {
        Ok(rule) => Ok(Some(rule)),
        Err(error)
            if matches!(
                error.code().0 as u32,
                0x8007_0002 | 0x8007_0490 | 0x8007_0003
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn open_rules() -> windows::core::Result<(
    ComApartment,
    windows::Win32::NetworkManagement::WindowsFirewall::INetFwRules,
)> {
    use windows::Win32::NetworkManagement::WindowsFirewall::{INetFwPolicy2, NetFwPolicy2};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

    let com = ComApartment::new()?;
    let policy: INetFwPolicy2 =
        unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)? };
    let rules = unsafe { policy.Rules()? };
    Ok((com, rules))
}

#[cfg(windows)]
struct ComApartment;

#[cfg(windows)]
impl ComApartment {
    fn new() -> windows::core::Result<Self> {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scope_has_a_stable_argument_and_wire_status() {
        assert_eq!(
            FirewallScope::PrivateLocalSubnet.status(),
            HostFirewallStatus::PrivateLocalSubnet
        );
        assert_eq!(
            FirewallScope::AllProfilesLocalSubnet.argument(),
            "all-profiles-local-subnet"
        );
        assert_eq!(FirewallScope::AllAddresses.argument(), "all-addresses");
    }

    #[test]
    fn missing_guidance_lists_every_explicit_choice() {
        let guidance = guidance(HostFirewallStatus::Missing);
        for scope in FirewallScope::ALL {
            assert!(guidance.contains(scope.argument()));
        }
        assert!(guidance.contains("firewall disable"));
    }
}
