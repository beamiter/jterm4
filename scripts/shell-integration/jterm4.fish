# jterm4 shell integration for fish.
# Source from ~/.config/fish/config.fish, for example:
#   if test "$TERM_PROGRAM" = jterm4; source /path/to/jterm4.fish; end

if set -q __jterm4_fish_loaded
    return 0
end
set -g __jterm4_fish_loaded 1

function __jterm4_osc
    printf '\033]%s\007' $argv[1]
end

function __jterm4_report_cwd --on-variable PWD
    set -l host (hostname 2>/dev/null; or echo localhost)
    set -l enc (string escape --style=url -- $PWD)
    __jterm4_osc "7;file://$host$enc"
end

function __jterm4_prompt_start  ; __jterm4_osc "133;A" ; end
function __jterm4_prompt_end    ; __jterm4_osc "133;B" ; end
function __jterm4_command_start ; __jterm4_osc "133;C" ; end
function __jterm4_command_end   ; __jterm4_osc "133;D;$argv[1]" ; end

function __jterm4_preexec --on-event fish_preexec
    __jterm4_command_start
end

function __jterm4_postexec --on-event fish_postexec
    __jterm4_command_end $status
end

if not functions -q __jterm4_orig_prompt
    functions -c fish_prompt __jterm4_orig_prompt
    function fish_prompt
        __jterm4_prompt_start
        __jterm4_orig_prompt
        __jterm4_prompt_end
    end
end

__jterm4_report_cwd
set -gx TERM_PROGRAM jterm4
