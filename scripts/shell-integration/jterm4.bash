# jterm4 shell integration for bash.
# Source from ~/.bashrc, for example:
#   [[ $TERM_PROGRAM == jterm4 ]] && source /path/to/jterm4.bash

[[ -n ${__JTERM4_BASH_LOADED:-} ]] && return 0
__JTERM4_BASH_LOADED=1
__jterm4_integration_source=${BASH_SOURCE[0]}

__jterm4_osc() { printf '\033]%s\007' "$1"; }
__jterm4_prompt_start() { __jterm4_osc "133;A"; }
__jterm4_prompt_end() { __jterm4_osc "133;B"; }
__jterm4_command_start() { __jterm4_osc "133;C"; }
__jterm4_command_end() { __jterm4_osc "133;D;$1"; }

__jterm4_report_cwd() {
    local host=${HOSTNAME:-localhost}
    local out= i ch
    LC_ALL=C
    for ((i = 0; i < ${#PWD}; i++)); do
        ch=${PWD:i:1}
        case $ch in
            [A-Za-z0-9._~/-]) out+=$ch ;;
            *) printf -v out '%s%%%02X' "$out" "'$ch" ;;
        esac
    done
    __jterm4_osc "7;file://${host}${out}"
}

__jterm4_in_command=0
__jterm4_in_prompt_command=0

__jterm4_preexec() {
    [[ -n ${COMP_LINE:-} ]] && return
    [[ ${BASH_SOURCE[1]:-} == "$__jterm4_integration_source" ]] && return

    # DEBUG fires before PROMPT_COMMAND and, with functrace enabled, inside its
    # functions too. Mark the complete prompt phase here so neither our hook nor
    # a user's saved PROMPT_COMMAND is mistaken for a submitted shell command.
    if [[ ${BASH_COMMAND} == "__jterm4_prompt_command" ]]; then
        __jterm4_in_prompt_command=1
        return
    fi
    (( __jterm4_in_prompt_command == 1 )) && return

    if (( __jterm4_in_command == 0 )); then
        __jterm4_in_command=1
        __jterm4_command_start
    fi
}

__jterm4_precmd() {
    local ec=$1
    if (( __jterm4_in_command == 1 )); then
        __jterm4_command_end "$ec"
        __jterm4_in_command=0
    fi
    __jterm4_report_cwd
    __jterm4_prompt_start
    if [[ -z ${__JTERM4_PS1_HOOKED:-} ]]; then
        PS1="${PS1}\[$(__jterm4_prompt_end)\]"
        __JTERM4_PS1_HOOKED=1
    fi
}

# Preserve every existing prompt hook, including Bash 5's array form, while
# making our dispatcher the sole PROMPT_COMMAND visible to the DEBUG trap.
__jterm4_saved_prompt_commands=("${PROMPT_COMMAND[@]:-}")
__jterm4_prompt_command() {
    local ec=$?
    local command
    __jterm4_in_prompt_command=1
    __jterm4_precmd "$ec"
    for command in "${__jterm4_saved_prompt_commands[@]}"; do
        [[ -n $command ]] && builtin eval -- "$command"
    done
    __jterm4_in_prompt_command=0
}

unset PROMPT_COMMAND
PROMPT_COMMAND=__jterm4_prompt_command
export TERM_PROGRAM=jterm4
trap '__jterm4_preexec' DEBUG
