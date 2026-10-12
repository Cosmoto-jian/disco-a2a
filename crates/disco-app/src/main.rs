#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    // Agents launch this same binary as their stdio MCP server (see mcp.rs).
    if std::env::args().nth(1).as_deref() == Some("--mcp") {
        disco_app_lib::mcp_stdio();
        return;
    }
    disco_app_lib::run();
}
