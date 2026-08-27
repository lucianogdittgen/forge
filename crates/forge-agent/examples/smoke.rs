//! Live smoke test: spawn the real `claude` CLI through Forge's driver and
//! confirm the lockdown flags actually take effect end to end.

use forge_agent::claude::{ClaudeAgent, ClaudeAgentConfig};
use forge_agent::{Agent, AgentEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut cfg = ClaudeAgentConfig::new(std::env::current_dir()?);
    cfg.model = "claude-sonnet-5".into();
    cfg.system_prompt = Some("Answer in one short sentence.".into());

    println!("argv: claude {}", cfg.argv().join(" "));
    let mut agent = ClaudeAgent::spawn(cfg).await?;

    agent.send("Reply with exactly: FORGE-OK").await?;

    let mut text = String::new();
    let mut saw_ready = false;
    while let Some(ev) = agent.next_event().await {
        match ev {
            AgentEvent::Ready { tools } => {
                if saw_ready { continue; }
                saw_ready = true;
                println!("[ready] tool surface: {tools:?}");
                // The Q1 claim, verified in Forge's own code path.
                let builtins: Vec<_> = tools
                    .iter()
                    .filter(|t| !t.starts_with("mcp__forge__"))
                    .collect();
                if builtins.is_empty() {
                    println!("[ready] LOCKDOWN OK - zero built-in tools");
                } else {
                    anyhow::bail!("built-in tools leaked through: {builtins:?}");
                }
            }
            AgentEvent::Text(t) => {
                println!("[text] {t}");
                text.push_str(&t);
            }
            AgentEvent::ToolCall { name, .. } => println!("[tool] {name}"),
            AgentEvent::Warning(w) => eprintln!("[stderr] {w}"),
            AgentEvent::TurnFinished { session, cost_usd, is_error } => {
                println!("[done] session={} cost={cost_usd:?} err={is_error}", session.0);
                break;
            }
            _ => {}
        }
    }

    if text.contains("FORGE-OK") {
        println!("\nSMOKE TEST PASSED");
        Ok(())
    } else {
        anyhow::bail!("did not get expected reply; got {text:?}")
    }
}
