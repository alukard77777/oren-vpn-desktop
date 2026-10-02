//! Is the system clock right? Compared with the `Date` header of a well-known HTTPS site
//! (plain HTTP as a last resort: a clock off by months fails every TLS handshake).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tried in order; the first `Date` header wins. Only the header is read.
pub const TIME_SOURCES: [&str; 3] = ["https://ya.ru/", "https://www.google.com/", "http://ya.ru/"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

/// Seconds since the epoch of an RFC 9110 IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub fn parse_http_date(value: &str) -> Option<i64> {
    let fields = value.split_whitespace().collect::<Vec<_>>();
    let [_weekday, day, month, year, time, "GMT"] = fields.as_slice() else {
        return None;
    };
    let day = day
        .parse::<u32>()
        .ok()
        .filter(|day| (1..=31).contains(day))?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| name == month)? as u32
        + 1;
    let year = year.parse::<i64>().ok().filter(|year| *year >= 1970)?;
    let mut clock = time.split(':').map(|part| part.parse::<i64>().ok());
    let (Some(Some(hours)), Some(Some(minutes)), Some(Some(seconds)), None) =
        (clock.next(), clock.next(), clock.next(), clock.next())
    else {
        return None;
    };
    if hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

/// How far this clock is ahead of the server (negative: behind), measured at the middle of the
/// request; `None` when no source answered with a usable `Date`.
pub async fn measure_skew() -> Option<i64> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;
    for source in TIME_SOURCES {
        let sent = SystemTime::now();
        let Ok(response) = client.head(source).send().await else {
            continue;
        };
        let received = SystemTime::now();
        let Some(server) = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_http_date)
        else {
            continue;
        };
        let local = (unix_seconds(sent) + unix_seconds(received)) / 2;
        return Some(local - server);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_http_date;

    #[test]
    fn imf_fixdates_parse_and_anything_else_does_not() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Fri, 02 Oct 2026 12:00:00 GMT"),
            Some(1_790_942_400)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Tue, 29 Feb 2028 23:59:59 GMT"),
            Some(1_835_481_599)
        );
        for bad in [
            "",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1994 08:49 GMT",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
    }
}
