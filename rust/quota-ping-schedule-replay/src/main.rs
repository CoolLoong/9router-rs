use std::io::{self, Read};

use jiff::Timestamp;
use serde_json::Value;

#[path = "../../app/src/quota_ping_schedule.rs"]
mod quota_ping_schedule;

use quota_ping_schedule::Schedule;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: Value = serde_json::from_str(&input)?;
    let schedule = Schedule::from_value(&input["schedule"]);
    let times = input["times"].as_array().ok_or("Missing times")?;
    let mut slots = Vec::with_capacity(times.len());
    for time in times {
        let timestamp: Timestamp = time.as_str().ok_or("Expected an ISO timestamp")?.parse()?;
        slots.push(
            schedule
                .as_ref()
                .and_then(|s| s.due_slot(timestamp.as_millisecond())),
        );
    }
    println!("{}", serde_json::to_string(&slots)?);
    Ok(())
}
