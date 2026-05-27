pub mod common;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use self::windows::run_vm;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use self::linux::run_vm;

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod stub;
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub use self::stub::run_vm;
