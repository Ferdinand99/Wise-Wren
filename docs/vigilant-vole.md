# Vigilant Vole

With [Vigilant Vole](https://hedgerow.tools/Vigilant-Vole/) connected, an update doesn't show as an outage on your
status pages, and VV's monitors become part of the health check:

- every run opens a **maintenance window** in VV for the target's linked monitors (and the status pages that show
  them) when its preflight is through, and closes it a minute after the run ends, also after a rollback;
- a **scheduled** update shows in VV as planned maintenance ahead of time, and moves or disappears with the
  schedule;
- during the health check Wren reads the monitors' **real results during maintenance**: the update counts as
  healthy only when every linked monitor is up for the health period, and a monitor that stays down rolls it back.

## Needs Vigilant Vole 0.9.0 or later

VV 0.9.0 reports a monitor's real result during maintenance (`raw_status`). With an older VV the monitor check is
skipped and the container checks decide alone; Test connection and the run log say "Vigilant Vole is too old to
report results during maintenance; update to 0.9.0 or later".

## Connecting

1. In Vigilant Vole: Settings → API tokens → create a token with access **write**, for a user whose role is
   **editor** or **admin** (a read token, or a viewer's, can't change maintenance windows).
2. In Wren: Settings → Vigilant Vole (admins): VV's address (for example `http://10.0.3.26:3001`) and the token.
   The token is sealed with the instance key and never shown again; leave the field empty to keep it.
3. **Test connection** checks the token, VV's version and its monitors.

Then link monitors on each target's page (admins): single monitors, or a **tag** (every monitor with that tag at
run time). A target without links relies on its container checks, and its page says so.

## When VV can't be reached

The run goes on without a window (the run's Maintenance window step says why, and a notification goes out), unless
the target is set to **Require Vigilant Vole**: then the run is cancelled before anything stops. During the health
check, three failed attempts in a row leave the decision to the container checks.

The agents never talk to VV and never see its token.
