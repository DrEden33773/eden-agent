// Modified by Eden Agent for native host integration; see the accompanying EDEN-FRONTEND.md.
//! Eden-only management entrypoints; values are collected in transient modals.
use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};
macro_rules! management_command {
    ($type:ident, $name:literal, $description:literal, $usage:literal) => {
        pub struct $type;
        impl SlashCommand for $type {
            slash_meta! { name: $name, description: $description, usage: $usage, session_scoped: true, }
            fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
                if !args.trim().is_empty() { return CommandResult::Error("Open the command without arguments; enter values in its form".into()); }
                CommandResult::Action(Action::EdenOpen($name))
            }
        }
    };
}
management_command!(
    Auth,
    "auth",
    "Provider API-key and OAuth authentication",
    "/auth"
);
management_command!(
    Config,
    "config",
    "Business configuration for this Session",
    "/config"
);
management_command!(
    Sessions,
    "sessions",
    "Read, rename or migrate saved sessions",
    "/sessions"
);
management_command!(
    Resources,
    "resources",
    "Discover and reload this Session's skills and templates",
    "/resources"
);
