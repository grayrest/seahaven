## Regular expressions, backed by the Rust [`regex`](https://docs.rs/regex) crate.
##
## Patterns use that crate's syntax, which is a linear-time engine without
## backreferences or lookaround -- a pattern relying on either will fail to
## compile rather than behave differently.
import Host

Regex :: [].{

	## Does `pattern` match anywhere in `haystack`?
	##
	## Returns `Err(RegexErr(message))` when the pattern does not compile, with
	## the crate's own diagnostic as the message.
	##
	## ```roc
	## Regex.is_match!("^a.c$", "abc")? # Bool.True
	## ```
	is_match! : Str, Str => Try(Bool, [RegexErr(Str), ..])
	is_match! = |pattern, haystack|
		# Re-tagged rather than passed through: the hosted signature is a CLOSED
		# union, which would not unify with an app's own error type.
		match Host.regex_is_match!(pattern, haystack) {
			Ok(value) => Ok(value)
			Err(RegexErr(message)) => Err(RegexErr(message))
		}

	## Replace every match of `pattern` in `haystack` with `replacement`.
	##
	## `$1`, `$2`, and `$name` in the replacement refer to capture groups.
	##
	## ```roc
	## Regex.replace_all!("a+", "aaabaaa", "-")? # "-b-"
	## ```
	replace_all! : Str, Str, Str => Try(Str, [RegexErr(Str), ..])
	replace_all! = |pattern, haystack, replacement|
		match Host.regex_replace_all!(pattern, haystack, replacement) {
			Ok(value) => Ok(value)
			Err(RegexErr(message)) => Err(RegexErr(message))
		}
}
