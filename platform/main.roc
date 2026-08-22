## flatland: the confined platform for rocjust.
##
## A native command-line platform with a filesystem, process, environment,
## signal, clock and RNG surface -- every effect routed through the sandbox
## (`brush-platform`). Adapted from `basic-cli`, trimmed to what a confined
## recipe runner needs: no network (D9), no sqlite, no sleep, no locale.
platform ""
	requires {
		main! : List([Utf8(Str), UnixBytes(List(U8)), WindowsU16s(List(U16))]) => Try({}, [Exit(I32), ..])
	}
	exposes [Cmd, Env, IOErr, OsStr, Path, Random, Regex, Signal, Stdin, Stdout, Stderr, Tty, Utc]
	packages {}
	provides { "roc_main": main_for_host! }
	hosted {
		"hosted_cmd_exec_exit_code": Host.cmd_exec_exit_code!,
		"hosted_cmd_exec_output": Host.cmd_exec_output!,
		"hosted_dir_create": Host.dir_create!,
		"hosted_dir_create_all": Host.dir_create_all!,
		"hosted_dir_delete_empty": Host.dir_delete_empty!,
		"hosted_dir_delete_all": Host.dir_delete_all!,
		"hosted_dir_list": Host.dir_list!,
		"hosted_env_var": Host.env_var!,
		"hosted_env_cwd": Host.env_cwd!,
		"hosted_env_exe_path": Host.env_exe_path!,
		"hosted_env_temp_dir": Host.env_temp_dir!,
		"hosted_file_read_bytes": Host.file_read_bytes!,
		"hosted_file_write_bytes": Host.file_write_bytes!,
		"hosted_file_read_utf8": Host.file_read_utf8!,
		"hosted_file_write_utf8": Host.file_write_utf8!,
		"hosted_file_delete": Host.file_delete!,
		"hosted_file_size_in_bytes": Host.file_size_in_bytes!,
		"hosted_file_is_executable": Host.file_is_executable!,
		"hosted_file_is_readable": Host.file_is_readable!,
		"hosted_file_is_writable": Host.file_is_writable!,
		"hosted_file_time_accessed": Host.file_time_accessed!,
		"hosted_file_time_modified": Host.file_time_modified!,
		"hosted_file_time_created": Host.file_time_created!,
		"hosted_path_type": Host.path_type!,
		"hosted_random_seed_u64": Host.random_seed_u64!,
		"hosted_random_seed_u32": Host.random_seed_u32!,
		"hosted_stderr_line": Host.stderr_line!,
		"hosted_stderr_write": Host.stderr_write!,
		"hosted_stderr_write_bytes": Host.stderr_write_bytes!,
		"hosted_stdin_line": Host.stdin_line!,
		"hosted_stdin_bytes": Host.stdin_bytes!,
		"hosted_stdin_read_to_end": Host.stdin_read_to_end!,
		"hosted_stdout_line": Host.stdout_line!,
		"hosted_stdout_write": Host.stdout_write!,
		"hosted_stdout_write_bytes": Host.stdout_write_bytes!,
		"hosted_tty_enable_raw_mode": Host.tty_enable_raw_mode!,
		"hosted_tty_disable_raw_mode": Host.tty_disable_raw_mode!,
		"hosted_utc_now": Host.utc_now!,
		"hosted_file_hard_link": Host.file_hard_link!,
		"hosted_file_rename": Host.file_rename!,
		"hosted_env_platform": Host.env_platform!,
		"hosted_env_dict": Host.env_dict!,
		"hosted_env_set_cwd": Host.env_set_cwd!,
		"hosted_file_set_executable": Host.file_set_executable!,
		"hosted_regex_is_match": Host.regex_is_match!,
		"hosted_regex_replace_all": Host.regex_replace_all!,
		"hosted_env_tz_offset": Host.env_tz_offset!,
		"hosted_signal_install_handler": Host.signal_install_handler!,
		"hosted_signal_take": Host.signal_take!,
		"hosted_env_pid": Host.env_pid!,
		"hosted_env_num_cpus": Host.env_num_cpus!,
		"hosted_cmd_exec_status": Host.cmd_exec_status!,
		"hosted_path_canonicalize": Host.path_canonicalize!,
		"hosted_tty_is_terminal": Host.tty_is_terminal!,
		"hosted_cmd_exec_output_inherit_stdin": Host.cmd_exec_output_inherit_stdin!,
	}
	targets: {
		inputs_dir: "targets/",
		arm64mac: { inputs: ["libhost.a", app] },
		x64mac: { inputs: ["libhost.a", app] },
	}

import Cmd
import Env
import Host
import IOErr
import OsStr
import Path
import Random
import Regex
import Signal
import Stdin
import Stdout
import Stderr
import Tty
import Utc

main_for_host! : List(OsStr.OsStr) => I32
main_for_host! = |args|
	match main!(args) {
		Ok({}) => 0
		Err(Exit(code)) => code
		Err(other) => {
			Stderr.line!("Program exited with error: ${Str.inspect(other)}") ?? {}
			1
		}
	}
