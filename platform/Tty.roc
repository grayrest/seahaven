import Host

## Provides functionality to change the behaviour of the terminal.
## This is useful for running an app like vim or a game in the terminal.
Tty :: [].{

	## Enable terminal [raw mode](https://en.wikipedia.org/wiki/Terminal_mode) to disable some default terminal behaviour.
	##
	## This leads to the following changes:
	## - Input will not be echoed to the terminal screen.
	## - Input will be sent straight to the program instead of being buffered (= collected) until the Enter key is pressed.
	## - Special keys like Backspace and CTRL+C will not be processed by the terminal driver but will be passed to the program.
	enable_raw_mode! : () => {}
	enable_raw_mode! = || Host.tty_enable_raw_mode!()

	## Revert terminal to default behaviour
	disable_raw_mode! : () => {}
	disable_raw_mode! = || Host.tty_disable_raw_mode!()

	## Which standard stream to ask about. They are asked about separately
	## because they are redirected separately: `prog | less` leaves stderr on
	## the terminal while stdout is a pipe.
	Stream : [Stdout, Stderr]

	## Whether that stream is attached to a terminal rather than to a pipe or a
	## file.
	##
	## This is what decides whether colour is wanted when the choice is left to
	## the program: a pipe usually wants the escape sequences left out, since
	## whatever reads it will not interpret them.
	is_terminal! : Stream => Bool
	is_terminal! = |stream|
		Host.tty_is_terminal!(
			match stream {
				Stdout => 1
				Stderr => 2
			},
		)
}
