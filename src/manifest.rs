/// Slack app manifest for one bot, in YAML.
pub fn manifest(name: &str) -> String {
    format!(
        r#"display_information:
  name: {name}
  description: prime-bot personality "{name}"
features:
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
  bot_user:
    display_name: {name}
    always_online: true
oauth_config:
  scopes:
    bot:
      - chat:write
      - channels:history
      - groups:history
      - im:history
      - channels:read
      - groups:read
      - im:read
      - users:read
      - reactions:write
      - channels:manage
      - groups:write
settings:
  event_subscriptions:
    bot_events:
      - message.channels
      - message.groups
      - message.im
      - member_joined_channel
  interactivity:
    is_enabled: false
  org_deploy_enabled: false
  socket_mode_enabled: true
  token_rotation_enabled: false
"#
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_is_valid_yaml() {
        let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&super::manifest("atlas")).unwrap();
        assert_eq!(
            v["settings"]["socket_mode_enabled"],
            serde_yaml_ng::Value::Bool(true)
        );
        assert_eq!(
            v["features"]["bot_user"]["display_name"].as_str(),
            Some("atlas")
        );
    }
}
