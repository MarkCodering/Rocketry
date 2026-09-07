use anyhow::Result;
use ratatui::{Terminal, backend::TestBackend};
use std::time::Instant;
fn main() -> Result<()> {
    let mut app = rocketry_tui::showcase();
    let card = app.cards.last().unwrap().clone();
    for _ in 0..1000 {
        app.cards.push(card.clone());
    }
    let mut terminal = Terminal::new(TestBackend::new(180, 50))?;
    terminal.draw(|f| rocketry_tui::render(f, &app))?;
    let mut times = vec![];
    for _ in 0..200 {
        let start = Instant::now();
        terminal.draw(|f| rocketry_tui::render(f, &app))?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({"terminal":"180x50","cards":app.cards.len(),"p95_frame_ms":times[190],"scope":"cached layout, TestBackend frame preparation"})
    );
    Ok(())
}
