import Host
import InternalDateTime

## Read UTC time and convert Unix timestamps into common representations.
Utc :: [].{

	## Get the current UTC time as nanoseconds since the Unix epoch (January 1, 1970).
	now! : () => U128
	now! = || Host.utc_now!() ?? 0

	## Convert nanoseconds since epoch to milliseconds since epoch.
	## The local timezone's offset from UTC, in seconds, right now.
	##
	## Read for the CURRENT instant rather than from a fixed zone rule, so it
	## already accounts for daylight saving. Negative west of Greenwich.
	tz_offset_seconds! : () => I64
	tz_offset_seconds! = || Host.env_tz_offset!()

	to_millis_since_epoch : U128 -> U128
	to_millis_since_epoch = |nanos| nanos // 1_000_000

	## Convert milliseconds since epoch to nanoseconds since epoch.
	from_millis_since_epoch : U128 -> U128
	from_millis_since_epoch = |millis| millis * 1_000_000

	## Convert a timestamp to nanoseconds since epoch.
	to_nanos_since_epoch : U128 -> U128
	to_nanos_since_epoch = |nanos| nanos

	## Convert nanoseconds since epoch to a timestamp.
	from_nanos_since_epoch : U128 -> U128
	from_nanos_since_epoch = |nanos| nanos

	## Calculate the difference between two timestamps in nanoseconds.
	delta_as_nanos : U128, U128 -> U128
	delta_as_nanos = |a, b| if a > b {
		a - b
	} else {
		b - a
	}

	## Calculate the difference between two timestamps in milliseconds.
	delta_as_millis : U128, U128 -> U128
	delta_as_millis = |a, b| {
		nanos = if a > b {
			a - b
		} else {
			b - a
		}
		nanos // 1_000_000
	}

	## Convert a timestamp to an ISO 8601 UTC string with second precision.
	to_iso_8601 : U128 -> Str
	to_iso_8601 = |nanos| {
		millis = to_millis_since_epoch(nanos)
		datetime = InternalDateTime.epoch_millis_to_datetime(millis)

		InternalDateTime.to_iso_8601(datetime)
	}
}
