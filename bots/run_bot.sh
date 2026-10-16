#!/usr/bin/env bash
# Keeps trend_bot.py running through crashes and exchange restarts; stops once the bot finishes its run.
#   nohup setsid ./run_bot.sh >/dev/null 2>&1 < /dev/null &
cd "$(dirname "$0")"
until python3 -u trend_bot.py >> bot.log 2>&1; do
  echo "$(date '+%m-%d %H:%M:%S') bot exited with code $?; restarting in 30 s" >> bot.log
  sleep 30
done
