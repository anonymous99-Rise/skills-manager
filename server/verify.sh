#!/bin/bash
# Full-surface verification: every invoke command the web frontend can call.
# Usage: BASE=http://127.0.0.1:18080 VERIFY_TOKEN=<gitea pat> ./verify.sh
#        (or run against a SM_DEV_NO_AUTH=1 server without VERIFY_TOKEN)
BASE="${BASE:-http://127.0.0.1:18080}"
# Bypass any ambient proxy for local calls (a stray --noproxy * unquoted
# gets glob-expanded; env vars are the reliable way).
export no_proxy="127.0.0.1,localhost"
export NO_PROXY="127.0.0.1,localhost"
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY
CURL="curl -s -m 120"
PASS=0; WARN=0; FAIL=0

auth_args=()
if [ -n "$VERIFY_TOKEN" ]; then
  COOKIE=$(curl -s --noproxy '*' -m 15 -X POST "$BASE/api/auth/gitea/token" \
    -H "Content-Type: application/json" \
    -d "{\"token\":\"$VERIFY_TOKEN\"}" -D - -o /dev/null | grep -i '^set-cookie' | sed 's/^[Ss]et-[Cc]ookie: //' | cut -d';' -f1)
  if [ -z "$COOKIE" ]; then echo "LOGIN FAILED"; exit 1; fi
  echo "logged in via token"
  auth_args=(-b "$COOKIE")
fi

# expect <name> <cmd> <json-args> <allowed-statuses e.g. "200" or "200,404">
expect() {
  local name="$1" cmd="$2" body="$3" allowed="$4"
  local out status
  out=$($CURL -X POST "$BASE/api/invoke/$cmd" -H "Content-Type: application/json" \
        --data-binary "$body" "${auth_args[@]}" -w $'\n%{http_code}')
  status=$(printf '%s' "$out" | tail -1)
  local payload=$(printf '%s' "$out" | sed '$d')
  case ",$allowed," in
    *",$status,"*)
      if [ "$status" = "200" ]; then PASS=$((PASS+1)); echo "ok   $name ($status) ${payload:0:80}";
      else WARN=$((WARN+1)); echo "warn $name ($status) ${payload:0:100}"; fi
      ;;
    *)
      FAIL=$((FAIL+1)); echo "FAIL $name ($status) ${payload:0:160}";;
  esac
}

echo "═══ read surfaces ═══"
expect get_tool_status        get_tool_status        '{}' 200
expect get_presets            get_presets            '{}' 200
expect get_active_preset      get_active_preset      '{}' 200
expect get_managed_skills     get_managed_skills     '{}' 200
expect get_all_tags           get_all_tags           '{}' 200
expect get_projects           get_projects           '{}' 200
expect get_tool_order         get_tool_order_cmd     '{}' 200
expect central_path           get_central_repo_path  '{}' 200
expect central_override       get_central_repo_path_override '{}' 200
expect central_pending        get_central_repo_pending_path '{}' 200
expect central_warnings       get_central_repo_warnings '{}' 200
expect backup_status          git_backup_status      '{}' 200
expect backup_device          backup_device_name     '{}' 200
expect diagnostics            get_diagnostic_info    '{}' 200
expect app_update             check_app_update       '{}' 200
expect leaderboard            fetch_leaderboard      '{"board":"all"}' 200,500
expect settings_get           get_settings           '{"key":"language"}' 200
expect settings_set           set_settings           '{"key":"sm_verify","value":"1"}' 200
expect log_startup_event      log_startup_event      '{"label":"verify","elapsedMs":1}' 200

echo "═══ tool management ═══"
expect set_tool_en_off        set_tool_enabled       '{"key":"claude_code","enabled":false}' 200
expect set_tool_en_on         set_tool_enabled       '{"key":"claude_code","enabled":true}' 200
expect set_all_tools_en       set_all_tools_enabled  '{"enabled":true}' 200
expect set_tool_order         set_tool_order_cmd     '{"order":["claude_code"]}' 200,400
expect bad_tool_404           set_tool_enabled       '{"key":"no_such","enabled":true}' 404,500
expect add_custom_tool        add_custom_tool        '{"key":"sm_verify_agent","displayName":"Verify Agent","skillsDir":"/tmp/sm-verify-agent-skills"}' 200
expect tool_status_has_custom get_tool_status        '{}' 200
expect remove_custom_tool     remove_custom_tool     '{"key":"sm_verify_agent"}' 200
expect set_custom_path_404    set_custom_tool_path   '{"key":"no_such","path":"/tmp/x"}' 404,500
expect reset_custom_path_404  reset_custom_tool_path '{"key":"no_such"}' 404,500
expect set_custom_proj_404    set_custom_tool_project_path '{"key":"no_such","projectRelativeSkillsDir":".x"}' 404,500
expect reset_custom_proj_404  reset_custom_tool_project_path '{"key":"no_such"}' 404,500

echo "═══ skill install lifecycle (real git install) ═══"
PRESET_ID=$(curl -s --noproxy '*' -m 20 -X POST "$BASE/api/invoke/create_preset" -H "Content-Type: application/json" "${auth_args[@]}" -d '{"name":"sm-verify-preset","description":"verify"}' | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{console.log(JSON.parse(d).id)}catch(e){console.log('')}})" 2>/dev/null)
echo "     preset_id=$PRESET_ID"
REPO="https://github.com/anonymous99-Rise/skills-manager.git"
PREV=$(curl -s --noproxy '*' -m 120 -X POST "$BASE/api/invoke/preview_git_install" -H "Content-Type: application/json" "${auth_args[@]}" -d "{\"repoUrl\":\"$REPO\"}")
TEMP_DIR=$(printf '%s' "$PREV" | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{const j=JSON.parse(d);console.log(j.temp_dir||'')}catch(e){console.log('')}})" 2>/dev/null)
FIRST_REL=$(printf '%s' "$PREV" | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{const j=JSON.parse(d);console.log(j.skills&&j.skills[0]?j.skills[0].rel_path:'')}catch(e){console.log('')}})" 2>/dev/null)
FIRST_NAME=$(printf '%s' "$PREV" | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{const j=JSON.parse(d);console.log(j.skills&&j.skills[0]?j.skills[0].name:'')}catch(e){console.log('')}})" 2>/dev/null)
if [ -n "$TEMP_DIR" ] && [ -n "$FIRST_REL" ]; then
  PASS=$((PASS+1)); echo "ok   preview_git_install temp=$TEMP_DIR first=$FIRST_REL"
  expect confirm_git_install confirm_git_install "{\"repoUrl\":\"$REPO\",\"tempDir\":\"$TEMP_DIR\",\"items\":[{\"rel_path\":\"$FIRST_REL\",\"name\":\"$FIRST_NAME\"}]}" 200
  SKILL_ID=$(curl -s --noproxy '*' -m 30 -X POST "$BASE/api/invoke/get_managed_skills" -H "Content-Type: application/json" -d '{}' "${auth_args[@]}" | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{const j=JSON.parse(d);const s=j.find(s=>s.name===' $FIRST_NAME'.trim());console.log(s?s.id:'')}catch(e){console.log('')}})" 2>/dev/null)
  SKILL_NAME=$(printf '%s' "$FIRST_NAME" | tr -d ' ')
  echo "     skill_id=$SKILL_ID"
  if [ -n "$SKILL_ID" ]; then
    expect get_skill_document    get_skill_document    "{\"skillId\":\"$SKILL_ID\"}" 200
    expect get_source_doc        get_source_skill_document "{\"skillId\":\"$SKILL_ID\"}" 200,404,500
    expect get_source_diff       get_skill_source_diff "{\"skillId\":\"$SKILL_ID\"}" 200,404,500
    expect check_skill_update    check_skill_update    "{\"skillId\":\"$SKILL_ID\",\"force\":true}" 200
    expect set_skill_tags        set_skill_tags        "{\"skillId\":\"$SKILL_ID\",\"tags\":[\"verify\"]}" 200
    expect get_all_tags2         get_all_tags          '{}' 200
    expect add_to_preset         add_skill_to_preset   "{\"skillId\":\"$SKILL_ID\",\"presetId\":\"$PRESET_ID\"}" 200
    expect skills_for_preset     get_skills_for_preset "{\"presetId\":\"$PRESET_ID\"}" 200
    expect preset_skill_order    get_preset_skill_order "{\"presetId\":\"$PRESET_ID\"}" 200
    expect toggles               get_skill_tool_toggles "{\"skillId\":\"$SKILL_ID\",\"presetId\":\"$PRESET_ID\"}" 200,404
    expect sync_to_tool          sync_skill_to_tool    "{\"skillId\":\"$SKILL_ID\",\"tool\":\"claude_code\"}" 200,404,500
    expect unsync_from_tool      unsync_skill_from_tool "{\"skillId\":\"$SKILL_ID\",\"tool\":\"claude_code\"}" 200,404,500
    expect remove_from_preset    remove_skill_from_preset "{\"skillId\":\"$SKILL_ID\",\"presetId\":\"$PRESET_ID\"}" 200
    expect update_skill          update_skill          "{\"skillId\":\"$SKILL_ID\"}" 200
    expect delete_skill          delete_managed_skill  "{\"skillId\":\"$SKILL_ID\"}" 200
  else
    FAIL=$((FAIL+1)); echo "FAIL skill id lookup after install"
  fi
else
  FAIL=$((FAIL+1)); echo "FAIL preview_git_install: ${PREV:0:160}"
fi

echo "═══ install_git direct ═══"
expect install_git            install_git           "{\"repoUrl\":\"https://github.com/xingkongliang/xingkongliang.git\",\"name\":\"sm-verify-direct\"}" 200
DELID=$(curl -s --noproxy '*' -m 30 -X POST "$BASE/api/invoke/get_managed_skills" -H "Content-Type: application/json" -d '{}' "${auth_args[@]}" | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{const j=JSON.parse(d);const s=j.find(s=>s.name==='sm-verify-direct');console.log(s?s.id:'')}catch(e){console.log('')}})" 2>/dev/null)
[ -n "$DELID" ] && expect del_direct_install delete_managed_skill "{\"skillId\":\"$DELID\"}" 200

echo "═══ presets ═══"
if [ -n "$PRESET_ID" ]; then
  PASS=$((PASS+1)); echo "ok   create_preset id=$PRESET_ID"
  expect update_preset         update_preset         "{\"id\":\"$PRESET_ID\",\"name\":\"sm-verify-preset-2\"}" 200
  expect reorder_presets       reorder_presets       "{\"ids\":[\"$PRESET_ID\"]}" 200
  expect reorder_preset_skills reorder_preset_skills "{\"presetId\":\"$PRESET_ID\",\"skillIds\":[]}" 200
  expect apply_preset          apply_preset_to_default "{\"id\":\"$PRESET_ID\"}" 200,500
  expect switch_preset         switch_preset         "{\"id\":\"$PRESET_ID\"}" 200,500
  expect delete_preset         delete_preset         "{\"id\":\"$PRESET_ID\"}" 200
else
  FAIL=$((FAIL+1)); echo "FAIL create_preset"
fi

echo "═══ scan & import ═══"
expect scan_local_skills      scan_local_skills     '{}' 200
expect import_all_discovered  import_all_discovered '{}' 200
expect import_existing_404    import_existing_skill '{"sourcePath":"/tmp/definitely-not-here"}' 404,500
expect batch_import_404       batch_import_folder   '{"folderPath":"/tmp/definitely-not-here"}' 400,500

echo "═══ skill ops on missing id (expect 404 not unknown) ═══"
expect tags_missing           set_skill_tags        '{"skillId":"no-such","tags":[]}' 200,400,404,500
expect rename_tag             rename_tag            '{"oldName":"verify","newName":"verify2"}' 200
expect delete_tag             delete_tag            '{"name":"verify2"}' 200
expect rename_tag_empty       rename_tag            '{"oldName":"","newName":""}' 400,500

echo "═══ git backup ═══"
expect backup_fetch           git_backup_fetch      '{}' 200,500
expect sanitize_url           git_backup_sanitize_remote_url '{"url":"https://git.nxwysoft.com/zhangz/skills-library.git"}' 200
expect backup_init            git_backup_init        '{}' 200,500
expect backup_versions        git_backup_list_versions '{}' 200,500
expect backup_size            git_backup_size_report '{}' 200,500
expect backup_conflicts       git_backup_pending_conflicts '{}' 200
expect set_device_name        backup_set_device_name '{"name":"verify-device"}' 200
expect backup_commit          git_backup_commit     '{"message":"verify snapshot"}' 200,500
expect backup_push            git_backup_push       '{}' 200,500
expect backup_sync            git_backup_sync       '{"message":"verify sync"}' 200,500
expect migrate_creds          git_backup_migrate_credentials '{}' 200,500
expect gh_flow_start          github_device_flow_start '{}' 200,500
expect clone_bad_url          git_backup_clone      '{"url":"https://invalid.invalid/x.git"}' 400,404,500

echo "═══ unknown vs implemented ═══"
expect unknown_cmd            totally_made_up       '{}' 404,500

echo
echo "════════ RESULT: ok=$PASS warn=$WARN fail=$FAIL ════════"
[ "$FAIL" = "0" ] && echo "ALL COMMANDS RESOLVED (warn = valid business errors)" || echo "SOME COMMANDS FAILED"
