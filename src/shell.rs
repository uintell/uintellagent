use clap::CommandFactory;
use clap_complete::{generate, Shell};
use std::io;

pub fn print_completions<C: CommandFactory>(shell: Shell) {
    let mut command = C::command();
    let name = command.get_name().to_string();
    generate(shell, &mut command, name, &mut io::stdout());
}

pub fn print_fish_init() {
    print!("{}", FISH_INIT);
}

const FISH_INIT: &str = r#"# UIntell Agent Fish integration.
function ua --description 'UIntell Agent prompt or TUI'
    if test (count $argv) -eq 0
        command uintell-agent --tui
        return $status
    end

    set -l command_names serve init skills skill-new db capabilities doctor task step route chain orchestrate evaluate completions fish-init history
    if string match --quiet -- '-*' $argv[1]; or contains -- $argv[1] $command_names
        command uintell-agent $argv
    else
        command uintell-agent --prompt (string join ' ' -- $argv)
    end
end

complete --command ua --wraps uintell-agent
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fish_function_wraps_cli_and_free_form_prompts() {
        assert!(FISH_INIT.contains("function ua"));
        assert!(FISH_INIT.contains("command uintell-agent --tui"));
        assert!(FISH_INIT.contains("command uintell-agent --prompt"));
        assert!(FISH_INIT.contains("complete --command ua --wraps uintell-agent"));
    }

    #[test]
    fn fish_function_knows_every_cli_subcommand() {
        for command in crate::Cli::command().get_subcommands() {
            assert!(
                FISH_INIT.contains(command.get_name()),
                "Fish function is missing CLI command {}",
                command.get_name()
            );
        }
    }
}
