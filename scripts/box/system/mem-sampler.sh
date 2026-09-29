#!/usr/bin/env bash
# Record top memory consumers so the next OOM can be attributed after the fact.
#
# The 2026-08-06 OOM storm killed rustc, ld, opencode, dbus and the user manager,
# but the kernel only reports the victims -- never what grew first. Sampling once
# a minute gives the run-up. Output goes to the journal (persistent, auto-rotated).
set -uo pipefail

# free -m row 2: "Mem: total used free shared buff/cache available"
read -r _ mem_total mem_used _ _ _ mem_avail < <(free -m | awk 'NR==2')
swap_line="$(free -m | awk 'NR==3 {printf "swap_used=%sM/%sM", $3, $2}')"

echo "mem_total=${mem_total}M mem_used=${mem_used}M mem_avail=${mem_avail}M ${swap_line}"

ps -eo rss=,pid=,user=,comm= --sort=-rss 2>/dev/null | head -10 | while read -r rss pid user comm; do
  echo "  $((rss / 1024))M pid=$pid user=$user $comm"
done
