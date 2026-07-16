# jterm4 shell integration for zsh.
# Source from ~/.zshrc, for example:
#   [[ $TERM_PROGRAM == jterm4 ]] && source /path/to/jterm4.zsh

[[ -n ${__JTERM4_ZSH_LOADED:-} ]] && return 0
__JTERM4_ZSH_LOADED=1

__jterm4_osc() { printf '\033]%s\007' "$1"; }
__jterm4_prompt_start() { __jterm4_osc "133;A"; }
__jterm4_prompt_end() { __jterm4_osc "133;B"; }
__jterm4_command_start() { __jterm4_osc "133;C"; }
__jterm4_command_end() { __jterm4_osc "133;D;$1"; }

__jterm4_report_cwd() {
    local host=${HOST:-${HOSTNAME:-localhost}}
    local out= i ch
    for ((i = 1; i <= ${#PWD}; i++)); do
        ch=${PWD[i]}
        case $ch in
            [A-Za-z0-9._~/-]) out+=$ch ;;
            *) printf -v out '%s%%%02X' "$out" "'$ch" ;;
        esac
    done
    __jterm4_osc "7;file://${host}${out}"
}

__jterm4_in_command=0
__jterm4_preexec() {
    if (( __jterm4_in_command == 0 )); then
        __jterm4_in_command=1
        __jterm4_command_start
    fi
}
__jterm4_precmd() {
    local ec=$?
    if (( __jterm4_in_command == 1 )); then
        __jterm4_command_end "$ec"
        __jterm4_in_command=0
    fi
    __jterm4_report_cwd
    __jterm4_prompt_start
}

if [[ -z ${__JTERM4_PS1_HOOKED:-} ]]; then
    PS1="${PS1}%{$(__jterm4_prompt_end)%}"
    __JTERM4_PS1_HOOKED=1
fi

autoload -Uz add-zsh-hook
add-zsh-hook preexec __jterm4_preexec
add-zsh-hook precmd __jterm4_precmd
export TERM_PROGRAM=jterm4
