//! The environment hosted effects, marshalled onto the session.
//!
//! These read D21's reduced environment and D15's session facts — never the host
//! process's own, which is the property the launcher milestone spent itself
//! establishing. The values cross as `Utf8` (D45).

use core::mem::ManuallyDrop;

use brush_platform::{PlatformEffects, TargetArch, TargetOs};

use crate::marshal::{host, io_err, native_from_str, native_into_string};
use crate::roc_platform_abi::{
    AARCH64OrARMOrOTHEROrX64OrX86 as Arch, AARCH64OrARMOrOTHEROrX64OrX86Payload as ArchPayload,
    AARCH64OrARMOrOTHEROrX64OrX86Tag as ArchTag, AnonStruct69eee2ff6c448fed as EnvPair,
    AnonStructBca0d23b5d625934 as EnvPlatform, EnvErrOrVarNotFound, EnvErrOrVarNotFoundPayload,
    EnvErrOrVarNotFoundTag, HostEnvCwdResult, HostEnvCwdResultPayload, HostEnvCwdResultTag,
    HostEnvExePathResult, HostEnvExePathResultPayload, HostEnvExePathResultTag,
    HostEnvSetCwdResult, HostEnvSetCwdResultPayload, HostEnvSetCwdResultTag, HostEnvVarResult,
    HostEnvVarResultPayload, HostEnvVarResultTag, LINUXOrMACOSOrOTHEROrWINDOWS as Os,
    LINUXOrMACOSOrOTHEROrWINDOWSPayload as OsPayload, LINUXOrMACOSOrOTHEROrWINDOWSTag as OsTag,
    RocHost, RocList, RocStr, UnixBytesOrUtf8OrWindowsU16s as Native,
};
use crate::with_session;

fn env_var_ok(value: Native) -> HostEnvVarResult {
    HostEnvVarResult {
        payload: HostEnvVarResultPayload {
            ok: ManuallyDrop::new(value),
        },
        tag: HostEnvVarResultTag::Ok,
    }
}

fn env_var_err(error: EnvErrOrVarNotFound) -> HostEnvVarResult {
    HostEnvVarResult {
        payload: HostEnvVarResultPayload {
            err: ManuallyDrop::new(error),
        },
        tag: HostEnvVarResultTag::Err,
    }
}

fn var_not_found(name: Native) -> EnvErrOrVarNotFound {
    EnvErrOrVarNotFound {
        payload: EnvErrOrVarNotFoundPayload {
            var_not_found: ManuallyDrop::new(name),
        },
        tag: EnvErrOrVarNotFoundTag::VarNotFound,
    }
}

fn env_var_io(error: &brush_platform::PlatformError, host: &RocHost) -> EnvErrOrVarNotFound {
    EnvErrOrVarNotFound {
        payload: EnvErrOrVarNotFoundPayload {
            env_err: ManuallyDrop::new(io_err(error, host)),
        },
        tag: EnvErrOrVarNotFoundTag::EnvErr,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_var(name: Native) -> HostEnvVarResult {
    let host = host();
    // A non-UTF-8 name cannot be looked up here (D45); report it as a plain
    // miss rather than inventing a variable.
    let name = match native_into_string(name, &host) {
        Ok(name) => name,
        Err(error) => return env_var_err(env_var_io(&error, &host)),
    };
    match with_session(None, |s| s.env_var(&name)) {
        Some(value) => env_var_ok(native_from_str(&value, &host)),
        None => env_var_err(var_not_found(native_from_str(&name, &host))),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_dict() -> RocList<EnvPair> {
    let host = host();
    let pairs = with_session(Vec::new(), |s| s.env_dict());
    let list = unsafe { RocList::<EnvPair>::allocate(pairs.len(), &host) };
    for (index, (name, value)) in pairs.iter().enumerate() {
        unsafe {
            list.elements.add(index).write(EnvPair {
                _0: native_from_str(name, &host),
                _1: native_from_str(value, &host),
            });
        }
    }
    list
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_cwd() -> HostEnvCwdResult {
    let host = host();
    let cwd = with_session(String::from("/"), |s| s.env_cwd());
    HostEnvCwdResult {
        payload: HostEnvCwdResultPayload {
            ok: ManuallyDrop::new(native_from_str(&cwd, &host)),
        },
        tag: HostEnvCwdResultTag::Ok,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_set_cwd(path: Native) -> HostEnvSetCwdResult {
    let host = host();
    let path = match native_into_string(path, &host) {
        Ok(path) => path,
        Err(error) => {
            return HostEnvSetCwdResult {
                payload: HostEnvSetCwdResultPayload {
                    err: ManuallyDrop::new(io_err(&error, &host)),
                },
                tag: HostEnvSetCwdResultTag::Err,
            };
        }
    };
    match with_session(Err(brush_platform::PlatformError::Unsupported), |s| {
        s.env_set_cwd(&path)
    }) {
        Ok(()) => HostEnvSetCwdResult {
            payload: HostEnvSetCwdResultPayload { ok: [] },
            tag: HostEnvSetCwdResultTag::Ok,
        },
        Err(error) => HostEnvSetCwdResult {
            payload: HostEnvSetCwdResultPayload {
                err: ManuallyDrop::new(io_err(&error, &host)),
            },
            tag: HostEnvSetCwdResultTag::Err,
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_temp_dir() -> Native {
    let host = host();
    let dir = with_session(String::from("/tmp"), |s| s.env_temp_dir());
    native_from_str(&dir, &host)
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_exe_path() -> HostEnvExePathResult {
    let host = host();
    let path = with_session(String::from(brush_platform::facts::EXE_PATH), |s| {
        s.env_exe_path()
    });
    HostEnvExePathResult {
        payload: HostEnvExePathResultPayload {
            ok: ManuallyDrop::new(native_from_str(&path, &host)),
        },
        tag: HostEnvExePathResultTag::Ok,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_platform() -> EnvPlatform {
    let host = host();
    // The default is unreachable (a session is always installed by `main`); a
    // launcher chooses the real target, which is why `PlatformTarget` has no
    // `Default` and this literal is local.
    let fallback = brush_platform::PlatformTarget {
        os: TargetOs::MacOs,
        arch: TargetArch::Aarch64,
    };
    let target = with_session(fallback, |s| s.env_platform());
    EnvPlatform {
        arch: arch(&target.arch, &host),
        os: os(&target.os, &host),
    }
}

fn arch(target: &TargetArch, host: &RocHost) -> Arch {
    let (payload, tag) = match target {
        TargetArch::Aarch64 => (ArchPayload { aarch64: [] }, ArchTag::AARCH64),
        TargetArch::Arm => (ArchPayload { arm: [] }, ArchTag::ARM),
        TargetArch::X64 => (ArchPayload { x64: [] }, ArchTag::X64),
        TargetArch::X86 => (ArchPayload { x86: [] }, ArchTag::X86),
        TargetArch::Other(name) => (
            ArchPayload {
                other: ManuallyDrop::new(RocStr::from_str(name, host)),
            },
            ArchTag::OTHER,
        ),
    };
    Arch { payload, tag }
}

fn os(target: &TargetOs, host: &RocHost) -> Os {
    let (payload, tag) = match target {
        TargetOs::Linux => (OsPayload { linux: [] }, OsTag::LINUX),
        TargetOs::MacOs => (OsPayload { macos: [] }, OsTag::MACOS),
        TargetOs::Windows => (OsPayload { windows: [] }, OsTag::WINDOWS),
        TargetOs::Other(name) => (
            OsPayload {
                other: ManuallyDrop::new(RocStr::from_str(name, host)),
            },
            OsTag::OTHER,
        ),
    };
    Os { payload, tag }
}
