/**
 * Auto-updater module — wraps @tauri-apps/plugin-updater with dialog prompts
 * and a fallback to the GitHub releases page when a silent install can't complete
 * (e.g. Windows SmartScreen blocking the unsigned NSIS installer, or Linux `.deb`/
 * `.rpm` installs, which the updater can't install in place and always route here;
 * only the Linux AppImage self-updates like the Windows/macOS installers).
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
 * Interactive:     install is attempted; if it fails (e.g. Windows SmartScreen blocks
 *                  the unsigned NSIS installer, or the platform's package format can't
 *                  self-update, as with Linux `.deb`/`.rpm`), the GitHub releases page
 *                  is opened and the user is guided to install the update manually.
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
      // Interactive fallback: SmartScreen, an unsupported package format (e.g. Linux
      // .deb/.rpm), or any other install failure — open the release page instead.
      const { open } = await import('@tauri-apps/plugin-shell');
      await open('https://github.com/RoienE/claude-overlay/releases/latest');
      await message(
        "Automatic update couldn't complete. The releases page has been opened — " +
        "download and install the latest version for your platform.",
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
