pub(crate) fn now_rfc1123_gmt() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        weekday(t.weekday()),
        t.day(),
        month(t.month()),
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn weekday(w: time::Weekday) -> &'static str {
    match w {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    }
}

fn month(m: time::Month) -> &'static str {
    match m {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    }
}
