import Host
import IOErr exposing [IOErr]
import OsStr exposing [OsStr]
import Path

## Read and modify the process environment without losing native OS strings.
##
## Variable names and values use [`OsStr`](OsStr) because Unix environment data
## is not required to be UTF-8. Use [`var_str!`](#var_str!) when an application
## specifically requires text. Paths use basic-cli's byte-preserving `Path` type.
Env :: [].{

	## Report the architecture and operating system for which the host was built.
	platform! : () => {
		arch : [X86, X64, ARM, AARCH64, OTHER(Str)],
		os : [LINUX, MACOS, WINDOWS, OTHER(Str)],
	}
	platform! = || Host.env_platform!()

	## Return all environment variables as native name/value pairs.
	##
	## Native non-Unicode values are preserved. Iteration order is unspecified
	## and may differ between calls or operating systems.
	## This process's own id.
	pid! : () => I64
	pid! = || Host.env_pid!()

	## Available parallelism, or 1 when it cannot be determined -- which is what
	## a cgroup-limited or otherwise opaque environment reports.
	num_cpus! : () => I64
	num_cpus! = || Host.env_num_cpus!()

	dict! : () => List((OsStr, OsStr))
	dict! = ||
		List.map(Host.env_dict!(), |(name, value)| (OsStr.from_raw(name), OsStr.from_raw(value)))

	## Reads the given environment variable.
	##
	## Returns `Err(VarNotFound(name))` if the variable is not set.
	var! : OsStr => Try(OsStr, [VarNotFound(OsStr), EnvErr(IOErr), ..])
	var! = |name|
		match Host.env_var!(OsStr.to_raw(name)) {
			Ok(raw) => Ok(OsStr.from_raw(raw))
			Err(VarNotFound(raw_name)) => Err(VarNotFound(OsStr.from_raw(raw_name)))
			Err(EnvErr(err)) => Err(EnvErr(err))
		}

	## Reads the given environment variable as a string if its native value is valid text.
	var_str! : OsStr => Try(Str, [VarNotFound(OsStr), EnvErr(IOErr), InvalidStr(U64), ..])
	var_str! = |name|
		match var!(name) {
			Ok(value) =>
				match OsStr.to_str_try(value) {
					Ok(str) => Ok(str)
					Err(InvalidStr(index)) => Err(InvalidStr(index))
				}
			Err(VarNotFound(raw_name)) => Err(VarNotFound(raw_name))
			Err(EnvErr(err)) => Err(EnvErr(err))
		}

	## Reads the [current working directory](https://en.wikipedia.org/wiki/Working_directory)
	## from the environment.
	##
	## Returns `Err(CwdUnavailable)` if the cwd cannot be determined.
	cwd! : () => Try(Path.Path, [CwdUnavailable, ..])
	cwd! = ||
		match Host.env_cwd!() {
			Ok(raw) => Ok(Path.from_raw(raw))
			Err(CwdUnavailable) => Err(CwdUnavailable)
		}

	## Change the process current working directory.
	##
	## Returns `Err(InvalidCwd(err))` when the path cannot be used as a working
	## directory. The process-wide change remains in effect until changed again.
	set_cwd! : Path.Path => Try({}, [InvalidCwd(IOErr), ..])
	set_cwd! = |path|
		Host.env_set_cwd!(Path.to_raw(path))
			.map_err(|err| InvalidCwd(err))

	## Gets the path to the currently-running executable.
	##
	## Returns `Err(ExePathUnavailable)` if the path cannot be determined.
	exe_path! : () => Try(Path.Path, [ExePathUnavailable, ..])
	exe_path! = ||
		match Host.env_exe_path!() {
			Ok(raw) => Ok(Path.from_raw(raw))
			Err(ExePathUnavailable) => Err(ExePathUnavailable)
		}

	## Gets the default directory for temporary files.
	temp_dir! : () => Path.Path
	temp_dir! = || Path.from_raw(Host.env_temp_dir!())
}
