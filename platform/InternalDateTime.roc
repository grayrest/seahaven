## Convert Unix timestamps into UTC calendar dates for public time formatting.
InternalDateTime :: [].{
	DateTime : {
		day : U128,
		hours : U128,
		minutes : U128,
		month : U128,
		seconds : U128,
		year : U128,
	}

	to_iso_8601 : DateTime -> Str
	to_iso_8601 = |{ year, month, day, hours, minutes, seconds }| {
		year_str = year_with_padded_zeros(year)
		month_str = two_digits(month)
		day_str = two_digits(day)
		hour_str = two_digits(hours)
		minute_str = two_digits(minutes)
		seconds_str = two_digits(seconds)

		"${year_str}-${month_str}-${day_str}T${hour_str}:${minute_str}:${seconds_str}Z"
	}

	epoch_millis_to_datetime : U128 -> DateTime
	epoch_millis_to_datetime = |millis| {
		seconds = millis // 1_000
		minutes = seconds // 60
		hours = minutes // 60

		normalize_date({
			year: 1970,
			month: 1,
			day: 1 + hours // 24,
			hours: hours % 24,
			minutes: minutes % 60,
			seconds: seconds % 60,
		})
	}

	year_with_padded_zeros : U128 -> Str
	year_with_padded_zeros = |year| {
		year_str = year.to_str()

		if year < 10 {
			"000${year_str}"
		} else if year < 100 {
			"00${year_str}"
		} else if year < 1000 {
			"0${year_str}"
		} else {
			year_str
		}
	}

	two_digits : U128 -> Str
	two_digits = |value| {
		value_str = value.to_str()

		if value < 10 {
			"0${value_str}"
		} else {
			value_str
		}
	}

	normalize_date : DateTime -> DateTime
	normalize_date = |current| {
		days_this_month = days_in_month(current.year, current.month)

		if current.day > days_this_month {
			normalize_date({
				..current,
				year: if current.month == 12 {
					current.year + 1
				} else {
					current.year
				},
				month: if current.month == 12 {
					1
				} else {
					current.month + 1
				},
				day: current.day - days_this_month,
			})
		} else {
			current
		}
	}

	days_in_month : U128, U128 -> U128
	days_in_month = |year, month|
		match month {
			1 => 31
			2 => if is_leap_year(year) {
				29
			} else {
				28
			}
			3 => 31
			4 => 30
			5 => 31
			6 => 30
			7 => 31
			8 => 31
			9 => 30
			10 => 31
			11 => 30
			12 => 31
			_ => 0
		}

	is_leap_year : U128 -> Bool
	is_leap_year = |year| {
		divisible_by_4 = year % 4 == 0
		divisible_by_100 = year % 100 == 0
		divisible_by_400 = year % 400 == 0

		if divisible_by_4 {
			if divisible_by_100 {
				divisible_by_400
			} else {
				True
			}
		} else {
			False
		}
	}
}
