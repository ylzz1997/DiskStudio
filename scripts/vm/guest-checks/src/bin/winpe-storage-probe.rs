//! Read-only WinPE capability probe. Print query status without disk metadata.
use std::collections::HashMap;
use std::process::ExitCode;
use wmi::{Variant, WMIConnection};

fn check(
    namespace: &str,
    connection: Result<WMIConnection, wmi::WMIError>,
    queries: &[(&str, &str)],
) -> bool {
    let connection = match connection {
        Ok(connection) => connection,
        Err(error) => {
            println!("{namespace} UNAVAILABLE {error}");
            return false;
        }
    };
    let mut available = true;
    for (class, query) in queries {
        match connection.raw_query::<HashMap<String, Variant>>(query) {
            Ok(_) => println!("{class} AVAILABLE"),
            Err(error) => {
                println!("{class} UNAVAILABLE {error}");
                available = false;
            }
        }
    }
    available
}

fn main() -> ExitCode {
    println!("DiskStudio WinPE storage providers");
    let cim = check(
        r"ROOT\CIMV2",
        WMIConnection::new(),
        &[
            (
                "Win32_DiskDrive",
                "SELECT Index,SerialNumber FROM Win32_DiskDrive",
            ),
            (
                "Win32_PageFileUsage",
                "SELECT Name FROM Win32_PageFileUsage",
            ),
        ],
    );
    let storage = r"ROOT\Microsoft\Windows\Storage";
    let smapi = check(
        storage,
        WMIConnection::with_namespace_path(storage),
        &[
            ("MSFT_Disk", "SELECT Number,Size FROM MSFT_Disk"),
            (
                "MSFT_Partition",
                "SELECT DiskNumber,PartitionNumber FROM MSFT_Partition",
            ),
            ("MSFT_Volume", "SELECT UniqueId FROM MSFT_Volume"),
            (
                "MSFT_PartitionToVolume",
                "SELECT __PATH FROM MSFT_PartitionToVolume",
            ),
        ],
    );
    println!(
        "Storage provider queries {}",
        if cim && smapi {
            "available"
        } else {
            "incomplete"
        }
    );
    ExitCode::from(u8::from(!(cim && smapi)))
}
