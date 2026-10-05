#!/usr/bin/env bash
# SDK-03 (Rails Active Storage): private attachments, signed direct upload,
# range download, existence check, prefix deletion. Run via scripts/interop.sh.
set -euo pipefail
case "$LITEBUCKET_ENDPOINT" in http://127.0.0.1:*|https://127.0.0.1:*) ;; *) echo "refusing non-local endpoint"; exit 1 ;; esac
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
APP="$ROOT/.interop/railsapp"
if [ ! -x "$APP/bin/rails" ]; then
  (cd "$ROOT/.interop" && rails new railsapp --minimal --skip-git --skip-bundle --skip-test \
     --skip-system-test --skip-docker --skip-kamal --skip-thruster --skip-rubocop --skip-brakeman \
     --skip-ci --database=sqlite3 >/dev/null)
  echo 'gem "aws-sdk-s3", require: false' >> "$APP/Gemfile"
  # Rails 8.1 calls JSON.parse(source, opts); keep the json 2.x line.
  echo 'gem "json", "~> 2.21"' >> "$APP/Gemfile"
  (cd "$APP" && bundle install --local >/dev/null)
fi
cd "$APP"
sed -i '' -e 's|^# require "active_job/railtie"|require "active_job/railtie"|' \
          -e 's|^# require "active_storage/engine"|require "active_storage/engine"|' config/application.rb
cp "$ROOT/tests/interop/rails/storage.yml" config/storage.yml
grep -q 'active_storage.service' config/environments/development.rb || \
  sed -i '' 's|^Rails.application.configure do|Rails.application.configure do\n  config.active_storage.service = :litebucket\n  config.active_job.queue_adapter = :inline|' config/environments/development.rb
ls db/migrate/*active_storage* >/dev/null 2>&1 || bin/rails active_storage:install >/dev/null
rm -f storage/development.sqlite3
bin/rails db:migrate >/dev/null
export LITEBUCKET_RAILS_BUCKET="rails-$(openssl rand -hex 4)"
bin/rails runner "$ROOT/tests/interop/rails/active_storage_check.rb"
