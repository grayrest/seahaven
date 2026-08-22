import IOErr exposing [IOErr]

## Declare the hosted effects and ABI-safe data exchanged with the native host.
##
## flatland's platform. Adapted from `basic-cli`'s `Host.roc`, trimmed to the
## effects a confined recipe runner needs: the network effects are gone by D9,
## and `sqlite`, `sleep`, `locale` and the file-reader handle are simply not in
## rocjust's surface. What remains is the ~50 effects the thirteen exposed
## modules reference, each routed through `brush-platform` on the Rust side.
Host :: [].{
	NativeOsStr : [Utf8(Str), UnixBytes(List(U8)), WindowsU16s(List(U16))]
	NativePath : [Utf8(Str), UnixBytes(List(U8)), WindowsU16s(List(U16))]

	Cmd : {
		args : List(NativeOsStr),
		clear_envs : Bool,
		envs : List(NativeOsStr),
		program : NativeOsStr,
	}

	CmdOutputSuccess : {
		stderr_bytes : List(U8),
		stdout_bytes : List(U8),
	}

	CmdOutputFailure : {
		stderr_bytes : List(U8),
		stdout_bytes : List(U8),
		exit_code : I32,
	}

	PathType : [File, Dir, SymLink, Other]

	cmd_exec_exit_code! : Cmd => Try(I32, IOErr)
	cmd_exec_output! : Cmd => Try(CmdOutputSuccess, [NonZeroExitCode(CmdOutputFailure), FailedToGetExitCode(IOErr)])

	dir_create! : NativePath => Try({}, [DirErr(IOErr)])
	dir_create_all! : NativePath => Try({}, [DirErr(IOErr)])
	dir_delete_empty! : NativePath => Try({}, [DirErr(IOErr)])
	dir_delete_all! : NativePath => Try({}, [DirErr(IOErr)])
	dir_list! : NativePath => Try(List(NativePath), [DirErr(IOErr)])

	env_var! : NativeOsStr => Try(NativeOsStr, [VarNotFound(NativeOsStr), EnvErr(IOErr)])
	env_cwd! : () => Try(NativePath, [CwdUnavailable])
	env_exe_path! : () => Try(NativePath, [ExePathUnavailable])
	env_temp_dir! : () => NativePath

	file_read_bytes! : NativePath => Try(List(U8), [FileErr(IOErr)])
	file_write_bytes! : NativePath, List(U8) => Try({}, [FileErr(IOErr)])
	file_read_utf8! : NativePath => Try(Str, [FileErr(IOErr)])
	file_write_utf8! : NativePath, Str => Try({}, [FileErr(IOErr)])
	file_delete! : NativePath => Try({}, [FileErr(IOErr)])
	file_size_in_bytes! : NativePath => Try(U64, [FileErr(IOErr)])
	file_is_executable! : NativePath => Try(Bool, [FileErr(IOErr)])
	file_is_readable! : NativePath => Try(Bool, [FileErr(IOErr)])
	file_is_writable! : NativePath => Try(Bool, [FileErr(IOErr)])
	file_time_accessed! : NativePath => Try(U128, [FileErr(IOErr)])
	file_time_modified! : NativePath => Try(U128, [FileErr(IOErr)])
	file_time_created! : NativePath => Try(U128, [FileErr(IOErr)])

	path_type! : NativePath => Try(PathType, IOErr)

	random_seed_u64! : () => Try(U64, [RandomErr(IOErr)])
	random_seed_u32! : () => Try(U32, [RandomErr(IOErr)])

	stderr_line! : Str => Try({}, [StderrErr(IOErr)])
	stderr_write! : Str => Try({}, [StderrErr(IOErr)])
	stderr_write_bytes! : List(U8) => Try({}, [StderrErr(IOErr)])

	stdin_line! : () => Try(Str, [EndOfFile, StdinErr(IOErr)])
	stdin_bytes! : () => Try(List(U8), [EndOfFile, StdinErr(IOErr)])
	stdin_read_to_end! : () => Try(List(U8), [StdinErr(IOErr)])

	stdout_line! : Str => Try({}, [StdoutErr(IOErr)])
	stdout_write! : Str => Try({}, [StdoutErr(IOErr)])
	stdout_write_bytes! : List(U8) => Try({}, [StdoutErr(IOErr)])

	tty_enable_raw_mode! : () => {}
	tty_disable_raw_mode! : () => {}

	# TODO(https://github.com/roc-lang/roc/issues/10163): revert to a bare U128
	# return once the compiler emits the clang/Rust u128 return convention on
	# x86_64-windows; bare U128 returns are currently misread there, while
	# Try-wrapped results cross the boundary correctly on every target.
	utc_now! : () => Try(U128, [ClockBeforeEpoch])

	file_hard_link! : NativePath, NativePath => Try({}, [FileErr(IOErr)])
	file_rename! : NativePath, NativePath => Try({}, [FileErr(IOErr)])
	env_platform! : () => {
		arch : [X86, X64, ARM, AARCH64, OTHER(Str)],
		os : [LINUX, MACOS, WINDOWS, OTHER(Str)],
	}
	env_dict! : () => List((NativeOsStr, NativeOsStr))
	env_set_cwd! : NativePath => Try({}, IOErr)
	file_set_executable! : NativePath, Bool => Try({}, [FileErr(IOErr)])
	## Regular expressions. A pattern that fails to compile comes back as
	## `RegexErr` with the engine's own message.
	regex_is_match! : Str, Str => Try(Bool, [RegexErr(Str)])
	regex_replace_all! : Str, Str, Str => Try(Str, [RegexErr(Str)])
	## The local timezone's offset from UTC, in seconds, RIGHT NOW -- so it
	## already accounts for daylight saving. Negative west of Greenwich.
	env_tz_offset! : () => I64
	## Arm the sandbox signal queue. Idempotent. The host's signals are NOT
	## forwarded (D47); the queue is fed by the sandbox.
	signal_install_handler! : () => {}
	## The first caught signal since the last call, or 0. Reading it CLEARS it,
	## so a poll loop sees each arrival exactly once. 0 in an ordinary run.
	signal_take! : () => I64
	## This session's own id -- session-scoped, not the host's (D15).
	env_pid! : () => I64
	## Available parallelism -- the job limit, not the host's core count.
	env_num_cpus! : () => I64
	## The child's exit code, or the NEGATED signal that killed it.
	cmd_exec_status! : Cmd => Try(I32, IOErr)
	## The path with every symlink and `.`/`..` resolved -- a VIRTUAL path.
	path_canonicalize! : NativePath => Try(NativePath, [CanonicalizeFailed])
	## Whether a file descriptor is attached to a terminal. Always false under
	## D36: the sandbox has no terminal.
	tty_is_terminal! : U64 => Bool
	## `exec_output!`, but with the child's stdin INHERITED rather than null --
	## the form just's backtick captures need.
	cmd_exec_output_inherit_stdin! : Cmd => Try(CmdOutputSuccess, [NonZeroExitCode(CmdOutputFailure), FailedToGetExitCode(IOErr)])
}
