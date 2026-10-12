# mino 的 zsh 集成片段。
#
# 由 mino 终端启动本地会话时注入环境变量 MINO_SHELL_INTEGRATION=1 后 source；
# 安装路径为 ~/.config/mino/shell/mino.zsh，由 mino-core 的
# `shell_integration::install` 写进 ~/.zshrc 的受管块里。
#
# 本文件只做四件事：
#   1. OSC 133 语义标记：A 提示符开始 / B 输入开始 / C 命令开始 / D 命令结束；
#   2. OSC 7 上报当前工作目录；
#   3. 加载 ~/.config/mino/shell/vendor 下的 zsh 插件；
#   4. Smart Tab 与可选的 starship 提示符。
#
# 约定：
#   * 仅在 MINO_SHELL_INTEGRATION=1 的交互式 zsh 中生效，在别的终端里手工
#     source 会立即返回，不产生任何副作用；
#   * 重复 source 不重复挂钩（加载标记 + add-zsh-hook 自身的去重语义）；
#   * 不依赖任何其它终端模拟器的专有逻辑（tmux 透传、用户变量上报等）。

# 守卫：非 mino 会话直接返回。不导出该变量，避免子 shell 误判自己在 mino 里。
[[ "${MINO_SHELL_INTEGRATION:-0}" == "1" ]] || return 0

# 非交互式 shell 不需要语义标记，也不应该往 stdout 写 OSC 序列。
[[ -o interactive ]] || return 0

# 幂等：同一 shell 内重复 source 时不再挂钩。
if [[ -n "${MINO_SHELL_INTEGRATION_LOADED:-}" ]]; then
  return 0
fi
MINO_SHELL_INTEGRATION_LOADED=1

# ---- 语义标记与工作目录上报 ----

# OSC 7：把当前工作目录告诉终端（cd 之后终端才能跟上，比如新建标签页
# 继承目录、窗口标题显示路径）。
__mino_osc7() {
  printf '\033]7;file://%s%s\033\\' "${HOST}" "${PWD}"
}

# 提示符标记：133;A 在 PS1 前部、133;B 在 PS1 末尾。
#
# %{...%} 告诉 zsh 这些字节不占显示宽度，否则提示符宽度会算多，长命令行
# 提前换行错位。不是只在 source 时包装一次，而是每条提示符都确认一遍：
# 之后任何重写 PS1/PROMPT 的代码（提示符框架会在自己的 precmd 钩子里整体
# 换掉提示符）都会把标记抹掉，重新包装的代价只有一次字符串比较。
__mino_mark_prompt() {
  [[ "${PS1-}" == *$'\033]133;A'* ]] && return 0
  PS1=$'%{\033]133;A\a%}'"${PS1-}"$'%{\033]133;B\a%}'
}

# 上一条命令是否由 preexec 标记过：首个提示符之前没有"命令结束"可言，
# 不该发 D 标记。
__mino_command_running=0

# precmd：命令结束标记（带退出码）+ 上报目录 + 包装提示符。
#
# zsh 在调用每个 precmd 钩子之前都会还原上一条命令的 $? 与 pipestatus
# （实测：排在前面钩子跑子进程、甚至 `return 7`，后面钩子读到的仍是用户
# 命令的退出码），所以在这里读 $? 不会被别人的钩子影响；但包装提示符必须
# 排在钩子列表最后，理由见文件末尾的挂钩说明。
__mino_precmd() {
  # 必须是函数里第一条语句：之后跑任何命令都会覆盖 $?。
  local exit_code=$?

  if (( ${__mino_command_running:-0} )); then
    printf '\033]133;D;%s\a' "${exit_code}"
    __mino_command_running=0
  fi

  __mino_osc7
  __mino_mark_prompt
}

# preexec：命令开始执行。
__mino_preexec() {
  printf '\033]133;C;\a'
  __mino_command_running=1
}

# ---- vendor 插件 ----
#
# 插件由 mino-core 的 `bootstrap_vendor` 克隆到 vendor 目录；这里只按固定
# 顺序加载，任一插件缺失都安静跳过，不影响其它插件和当前会话。
# 加载顺序有硬性要求：autosuggestions 最先（后面的插件要包装它定义的
# widget），syntax-highlighting 最后（它包装已有 widget 做高亮）。
__mino_vendor_dir="${HOME}/.config/mino/shell/vendor"

# zsh-autosuggestions：历史建议（POSTDISPLAY）。
if [[ -f "${__mino_vendor_dir}/zsh-autosuggestions/zsh-autosuggestions.zsh" ]]; then
  source "${__mino_vendor_dir}/zsh-autosuggestions/zsh-autosuggestions.zsh"
fi

# zsh-z：按访问频率跳目录。
if [[ -f "${__mino_vendor_dir}/zsh-z/zsh-z.plugin.zsh" ]]; then
  source "${__mino_vendor_dir}/zsh-z/zsh-z.plugin.zsh"
fi

# zsh-completions：只进 FPATH（补全函数按需 autoload，不需要 source）。
if [[ -d "${__mino_vendor_dir}/zsh-completions/src" ]]; then
  FPATH="${__mino_vendor_dir}/zsh-completions/src:${FPATH}"
  # 新增 FPATH 要重跑 compinit 才会进入补全索引；仅当用户本来就启用了
  # 补全系统时才补跑，-C 复用已有 dump（不重建、不弹目录安全检查提示）。
  # 用户没跑过 compinit 就保持原样，不替他开启补全系统。
  if (( ${+_comps} )); then
    autoload -Uz compinit && compinit -C
  fi
fi

# zsh-syntax-highlighting：命令语法高亮。
if [[ -f "${__mino_vendor_dir}/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh" ]]; then
  source "${__mino_vendor_dir}/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh"
fi

# ---- Smart Tab ----

# 模式优先级：MINO_SMART_TAB_MODE > MINO_SMART_TAB_DISABLE=1（等价 off）
# > 默认 suggestion_first。
#   suggestion_first：有内联建议先接受建议，否则走原生补全；
#   completion_first：先补全，当前词没有任何补全候选时才接受内联建议；
#   off             ：不接管 Tab，保持 zsh 原生 expand-or-complete。
if [[ -n "${MINO_SMART_TAB_MODE:-}" ]]; then
  __mino_smart_tab_mode="${MINO_SMART_TAB_MODE}"
elif [[ "${MINO_SMART_TAB_DISABLE:-0}" == "1" ]]; then
  __mino_smart_tab_mode=off
else
  __mino_smart_tab_mode=suggestion_first
fi

# 是否有可接受的内联建议：autosuggestions 用 POSTDISPLAY 展示建议，
# autosuggest-accept 是它定义的 widget。
__mino_has_suggestion() {
  [[ -n "${POSTDISPLAY:-}" ]] && (( ${+widgets[autosuggest-accept]} ))
}

# Tab widget（只有接管模式才会安装；off 时这个函数不会被绑定）。
# 不加 emulate：临时改用户选项会连带影响这里的补全（比如用户的 NO_BEEP
# 会在 widget 内被还原成默认值，每次 Tab 无候选就响铃）。
mino_smart_tab_widget() {
  case "${__mino_smart_tab_mode}" in
    suggestion_first)
      if __mino_has_suggestion; then
        zle autosuggest-accept
      else
        zle expand-or-complete
      fi
      ;;
    completion_first)
      # 补全跑完后 _lastcomp[nmatches] 是本次候选数：为 0（或补全系统没
      # 参与、该键不存在）说明这个位置根本没有候选，只补全会响铃，才退回
      # 接受建议。候选需要列出的场景 nmatches > 0，保持补全行为。
      zle expand-or-complete
      if (( ${_lastcomp[nmatches]:-0} == 0 )) && __mino_has_suggestion; then
        zle autosuggest-accept
      fi
      ;;
  esac
}

case "${__mino_smart_tab_mode}" in
  suggestion_first | completion_first)
    zle -N mino_smart_tab_widget
    # 把 widget 排除在 autosuggestions 的重绑定之外：不以 `_` 开头的名字会
    # 被它当成"可能修改缓冲区"的 widget 包装（包装后它会先清掉建议再调用
    # 原 widget，suggestion_first 就永远落到补全）。该数组在首个提示符时才
    # 被读取，这里追加不影响插件自己的默认项；插件不存在时也只是无害赋值。
    typeset -ga ZSH_AUTOSUGGEST_IGNORE_WIDGETS
    ZSH_AUTOSUGGEST_IGNORE_WIDGETS+=(mino_smart_tab_widget)
    # emacs / viins 是两种编辑模式各自的插入态 keymap；main 由 zsh 链接到
    # 其中之一，不需要单独绑定。
    for __mino_keymap in emacs viins; do
      bindkey -M "${__mino_keymap}" '^I' mino_smart_tab_widget
    done
    ;;
  *)
    # off：恢复两种编辑模式各自的原生 Tab 绑定。
    for __mino_keymap in emacs viins; do
      bindkey -M "${__mino_keymap}" '^I' expand-or-complete
    done
    ;;
esac
unset __mino_keymap

# ---- starship（可选）----
#
# 只有 starship 和 mino 专用配置都在时才接管提示符：预设写在 mino 自己的
# 配置目录里，用 STARSHIP_CONFIG 指向它，不碰用户的 ~/.config/starship.toml。
__mino_starship_config="${HOME}/.config/mino/starship.toml"
if command -v starship >/dev/null 2>&1 && [[ -f "${__mino_starship_config}" ]]; then
  export STARSHIP_CONFIG="${__mino_starship_config}"
  eval "$(starship init zsh)"
fi
unset __mino_starship_config

# ---- 挂钩 ----
#
# 放在文件末尾是有意的：本文件被追加在 ~/.zshrc 最后，注册在这里的 precmd
# 钩子排在用户配置（含上面 starship init 注册的钩子）之后，提示符标记由我们
# 最后包装，不会被别人改写。add-zsh-hook 内部就会去重，重复 source 也不会
# 重复挂钩（文件开头的加载标记是第二道保险）。
autoload -Uz add-zsh-hook
add-zsh-hook precmd __mino_precmd
add-zsh-hook preexec __mino_preexec
