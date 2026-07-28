# FSV — issue #1871: Chrome bridge UI repair misclassification and unconfirmable navigation

Host: `CABTOP`, Chrome 150.0.7871.187, 7 profiles, bridge extension in `Profile 5`.
Date: 2026-07-28. Commit `b9084aef`.

## Defect 2 — navigation confirmation and the ownership cascade

### Root cause

`Invoke-SynapseChromeAddressBarNavigation` confirmed navigation by comparing the
omnibox to the full target URL. Chrome normalises the displayed address for
`chrome://` pages and drops the query string, so
`chrome://extensions/?id=<id>&synapse_maintenance_token=<token>` can only ever
read back as `chrome://extensions`. The confirmation was therefore unsatisfiable.

The same assumption broke cleanup. `Close-SynapseOwnedChromeMaintenanceTabViaUi`
proved ownership by finding its token *in the address bar* — the exact string
Chrome had just erased — so a correctly-owned tab was disowned and orphaned.

### Fix

- Confirmation accepts the query/fragment-stripped address **and** now requires
  the document title to match `ExpectedTitlePattern`, which was previously
  recorded in error detail but never checked. Net evidence is stronger.
- Ownership is proven by the tab's exact UIA runtime identity
  (`owned_tab_runtime_id`, already in the lease and already trusted for
  selection), with the address kept as an alternative. Refusing `Ctrl+W` against
  a tab that cannot be proven owned is unchanged.
- Post-close confirmation additionally requires the owned runtime id to be
  absent from the strip; the old address-only test would pass merely because
  Chrome stripped the token.

### Verification

During the `b9084aef` deploy the bridge went `unavailable` after the reboot and
`synapse-setup.ps1` invoked the real existing-Chrome UI repair. That branch runs
`Enter-SynapseOwnedChromeMaintenanceTab` then navigation to the token URL
unconditionally — this defect's exact path.

```
Chrome bridge UI repair completed reason=existing_ready_extension_ui_reload_invoked
  active_profile=Profile 5 chrome_window_pid=11340 chrome_window_hwnd=393954
  ui_after={"title":"Extensions - Synapse Chrome Bridge - Google Chrome",
            "reload_button_present":true,"enable_toggle_on":true, ...}
Chrome bridge OK after existing-Chrome UI repair: stale=false capability=pageScreenshot
```

No `SYNAPSE_CHROME_NAVIGATION_NOT_CONFIRMED`, no
`SYNAPSE_CHROME_MAINTENANCE_CLEANUP_OWNERSHIP_LOST`.

Independent UIA readback of that exact hwnd, taken outside Synapse, proves the
cleanup half — the marker tab was closed and the operator's tab survived:

```
window hwnd=393954 title=Issues · ChrisRoyse/Synapse - Google Chrome
  TabContainerImpl count = 1
  tab count = 1
    tab: Issues · ChrisRoyse/Synapse
```

`chrome_bridge` health: `status=ok tab_control_available=true host_count=1`.

## Defect 1 — profile picker reported as a tab-strip failure

### Fix

`Read-SynapseChromeProfilePickerState` detects `ProfilePickerView` as the primary
witness, with the `RootView` heading only corroborating. The tab-strip read
raises `SYNAPSE_CHROME_MAINTENANCE_PROFILE_PICKER_ONLY` naming the offered
profiles, and keeps the original `chrome_tab_container_count=0` inside the detail
so nothing is lost for genuine tab-strip faults.

### Verification — against a real ProfilePickerView

A genuine picker was produced in a **fully isolated** `--user-data-dir` with two
synthetic profiles (`FSV Alpha`, `FSV Beta`), so the expected output was known
before running. The operator's Chrome and the live bridge were untouched. Both
functions were loaded out of the script's AST, so the deployed implementation is
what ran.

```
raw UIA:  ProfilePickerView present : True     TabContainerImpl count : 0

classifier -> { "present": true,
                "root_view_name": "Welcome to Chrome profiles",
                "profile_buttons": ["Open FSV Alpha profile","More actions for FSV Alpha",
                                    "Open FSV Beta profile","More actions for FSV Beta",
                                    "Add","Guest mode"] }

tab-strip read -> SYNAPSE_CHROME_MAINTENANCE_PROFILE_PICKER_ONLY
  detail={"hwnd":132520,"pid":14824,"window_title":"Google Chrome",
          "root_view_name":"Welcome to Chrome profiles",
          "offered_profiles":[...],"tab_strip_error":"chrome_tab_container_count=0"}
```

The heading read `Welcome to Chrome profiles`, **not** the `Who's using Chrome?`
string from the bug report — Chrome varies it. Detection was carried by the
`ProfilePickerView` class name, which is precisely why the heading is only a
corroborating witness.

Negative case: the real browser window (hwnd 393954) returns `present=false`, so
a genuine tab-strip fault still reports
`SYNAPSE_CHROME_MAINTENANCE_TABSTRIP_READ_FAILED`, now carrying
`profile_picker_present=false`.

### State restored

Isolated instance terminated and its user-data-dir deleted; operator Chrome
unchanged (pid 11340, single tab); `chrome_bridge` still `ok`.
