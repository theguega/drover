# bash completion for drover; installed by install.sh into bash-completion's directory.
_drover() {
  local cur prev cmd words i machine
  local commands="new attach link escalate brief agents worktrees usage screen reply keys scout mate send take release done recall journal config help"
  COMPREPLY=()
  cur=${COMP_WORDS[COMP_CWORD]}
  prev=${COMP_WORDS[COMP_CWORD-1]}
  cmd=${COMP_WORDS[1]}

  if ((COMP_CWORD == 1)); then
    COMPREPLY=($(compgen -W "$commands" -- "$cur"))
    return
  fi

  machine=
  for ((i = 2; i < COMP_CWORD; i++)); do
    [[ ${COMP_WORDS[i]} == --machine ]] && machine=${COMP_WORDS[i+1]}
  done

  case $prev in
    --machine) words=$(drover complete machines 2>/dev/null) ;;
    --mate) words=$(drover complete mates 2>/dev/null) ;;
    --thread) words=$(drover complete threads 2>/dev/null) ;;
    --pane) words=$(drover complete panes "$machine" 2>/dev/null) ;;
    rm) [[ $cmd == journal ]] && words=$(drover complete journal 2>/dev/null) ;;
    --agent|--model|--branch|--repo|--prompt|--text|--keys|--name) return ;;
    *)
      case $cmd in
        new) words="--machine --repo --branch --prompt --agent --model --scout --mate --name --discord" ;;
        attach) words="--machine --pane --discord" ;;
        link) words="--machine --pane --mate" ;;
        escalate|brief) words="--mate --text" ;;
        screen) words="--pane --thread --full" ;;
        take|release) words="--thread" ;;
        reply) words="--pane --thread" ;;
        keys) words="--thread --keys" ;;
        send) words="--thread --text" ;;
        scout) words="close --pane --discord" ;;
        done) words="--thread --remove --no-journal" ;;
        journal) words="rm --pane --thread --full" ;;
        recall) words="--full" ;;
        mate) words="$(drover complete mates 2>/dev/null) --discord" ;;
        help) words=$commands ;;
        *) words= ;;
      esac
      ;;
  esac
  COMPREPLY=($(compgen -W "$words" -- "$cur"))
}
complete -F _drover drover
