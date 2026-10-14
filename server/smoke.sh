#!/bin/bash
B=http://127.0.0.1:18080/api
post() {
  curl -s --noproxy '*' -m 60 -X POST "$B/invoke/$1" -H "Content-Type: application/json" --data-binary "$2"
  echo
}
echo "== get_presets";        post get_presets '{}'
echo "== create_preset";      post create_preset '{"name":"team-default","description":"smoke"}'
echo "== get_active_preset";  post get_active_preset '{}'
echo "== get_tool_status";    post get_tool_status '{}' | head -c 250; echo
echo "== get_managed_skills"; post get_managed_skills '{}'
echo "== get_projects";       post get_projects '{}'
echo "== get_settings";       post get_settings '{"key":"proxy_url"}'
echo "== set_settings";       post set_settings '{"key":"smoke_test","value":"1"}'
echo "== git_backup_status";  post git_backup_status '{}'
echo "== desktop_only";       post install_local '{"sourcePath":"/tmp"}'
echo "== unknown_cmd";        post no_such_cmd '{}'
echo "== tags";               post get_all_tags '{}'
echo "== check_updates";      post check_all_skill_updates '{}'
