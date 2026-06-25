/**
 * Auto-updater module — wraps @tauri-apps/plugin-updater with dialog prompts
 * and a SmartScreen-aware fallback to the GitHub releases page.
 *
 * Two code paths:
 *   interactive: false — silent startup / periodic check; all errors are swallowed.
 *   interactive: true  — user-initiated check; install failure opens the release page.
 */

import { check } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';
import { ask, message } from '@tauri-apps/plugin-dialog';

/** No-op placeholder reserved for future updater state initialisation. */
export function initUpdater(): void {
  // Nothing to initialise yet.
}

/**
 * Check for an available update and guide the user through installing it.
 *
 * Non-interactive: errors are swallowed — never shows dialogs for background checks.
 * Interactive:     install is attempted; if it fails (e.g. SmartScreen blocks the
 *                  unsigned NSIS installer), the GitHub releases page is opened and
 *                  the user is guided to run the installer manually.
 */
export async function checkForUpdates({ interactive }: { interactive: boolean }): Promise<void> {
  try {
    const update = await check();

    if (!update) {
      if (interactive) {
        await message("You're on the latest version.", { title: 'No updates', kind: 'info' });
      }
      return;
    }

    const yes = await ask(
      `Version ${update.version} is available. Install now?`,
      { title: 'Update available', kind: 'info' },
    );
    if (!yes) return;

    try {
      await update.downloadAndInstall();
      await relaunch();
    } catch {
      if (!interactive) return;
      // Interactive fallback: SmartScreen or install failure — open the release page.
      const { open } = await import('@tauri-apps/plugin-shell');
      await open('https://github.com/RoienE/claude-overlay/releases/latest');
      await message(
        "Automatic update couldn't complete. The download page has opened — run the " +
        "installer and choose 'More info' → 'Run anyway' to finish updating.",
        { title: 'Finish update manually', kind: 'warning' },
      );
    }
  } catch {
    // The updater treats any non-success response from the endpoint as an error.
    // Until a release with latest.json is published, the GitHub endpoint 404s, so
    // this fires on every check. Stay silent for background checks; show a friendly
    // dialog for interactive ones rather than leaving an unhandled rejection.
    if (interactive) {
      await message(
        "Couldn't check for updates right now. Please try again later.",
        { title: 'Update check failed', kind: 'error' },
      );
    }
  }
}
