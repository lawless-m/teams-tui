# Azure AD app registration for teams-tui

You are tenant admin, so this is a five-minute job. Do it before first run; the resulting `tenant_id` and `client_id` go into `~/.config/teams-tui/config.toml`.

## Steps

1. **Azure portal → Microsoft Entra ID → App registrations → New registration.**
   - Name: `teams-tui` (or whatever; only you will see it)
   - Supported account types: *Accounts in this organizational directory only (Single tenant)*
   - Redirect URI: leave blank. Device code flow does not use one.
   - Register.

2. **On the app's Overview page, copy:**
   - *Application (client) ID* → this is `client_id` in the config
   - *Directory (tenant) ID* → this is `tenant_id` in the config

3. **Authentication tab:**
   - Scroll down to *Advanced settings*.
   - Set *Allow public client flows* to **Yes**. This is required for device code flow.
   - Save.

4. **API permissions tab → Add a permission → Microsoft Graph → Delegated permissions.** Add each of these:
   - `User.Read`
   - `Chat.ReadWrite`
   - `ChannelMessage.Send`
   - `ChannelMessage.Read.All`
   - `Team.ReadBasic.All`
   - `Channel.ReadBasic.All`
   - `offline_access`

   `offline_access` is under a separate section (OpenId permissions) — scroll down to find it.

5. **Click *Grant admin consent for <tenant>*.** Confirm. All permissions should now show a green tick.

6. **Certificates & secrets tab:** nothing to do. Device code flow uses the client_id alone, no secret.

## Verifying

First run of the TUI will print something like:

```
To sign in, use a web browser to open https://microsoft.com/devicelogin
and enter the code ABC123XYZ to authenticate.
```

Open the URL, enter the code, consent to the scopes (you'll see them listed — they should match the list above). On success the TUI continues and begins polling.

The refresh token lands in `~/.config/teams-tui/token.json` (mode 0600) and is used for subsequent runs. Default refresh token lifetime is 90 days of inactivity in most tenants; if the TUI runs regularly, that's effectively indefinite.

## If something goes wrong

- **"AADSTS7000218: The request body must contain the following parameter: 'client_assertion' or 'client_secret'"** — you didn't enable *Allow public client flows*. Go back to Authentication tab.
- **"AADSTS65001: The user or administrator has not consented..."** — admin consent wasn't granted, or was granted but hasn't propagated yet (wait a minute). Check the API permissions tab for green ticks.
- **403 on `ChannelMessage.Read.All` calls** — this scope requires resource-specific consent (RSC) in some tenant configurations, or the delegated version may be admin-only. Since you're admin, re-check that admin consent was granted specifically for this scope.
- **401 after some time of working fine** — refresh token expired or was revoked. Run `/reauth` in the TUI to redo the device code flow.
