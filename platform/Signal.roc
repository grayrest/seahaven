## Fatal signals, for a program that wants to finish what it is doing before it
## dies.
##
## The default disposition kills the process at once, which orphans any child it
## has spawned. Installing these handlers RECORDS the signal instead, so a
## caller can let the child finish, report, and exit with the right code.
import Host

Signal :: [].{

	## Install handlers for SIGHUP, SIGINT and SIGQUIT. Idempotent, and a no-op
	## off unix.
	install! : () => {}
	install! = || Host.signal_install_handler!()

	## The first of those signals caught since the last call, or 0. Reading it
	## CLEARS it, so a poll loop sees each arrival exactly once.
	##
	## ```roc
	## match Signal.take!() {
	##     0 => keep_going!({})
	##     caught => report_and_exit!(Signal.name(caught), 128 + caught)
	## }
	## ```
	take! : () => I64
	take! = || Host.signal_take!()

	## `SIGINT` and friends, or `""` for anything this module does not install.
	name : I64 -> Str
	name = |signal|
		match signal {
			1 => "SIGHUP"
			2 => "SIGINT"
			3 => "SIGQUIT"
			15 => "SIGTERM"
			_ => ""
		}
}
