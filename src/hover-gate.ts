/** Decides when hovering a card may change the selection.
 *
 * When the overlay appears under a stationary cursor, Windows and Chromium
 * deliver a synthetic mouse move at the cursor's position. Selecting on that
 * would hijack the keyboard selection without the user touching the mouse,
 * so hover only counts once the pointer has really moved since the opening:
 * the first move records where the pointer is, and hover is allowed from the
 * first later move to a different screen position. `reset` starts over for
 * every new opening. */
export class HoverGate {
  private origin: { x: number; y: number } | null = null;
  private moved = false;

  reset(): void {
    this.origin = null;
    this.moved = false;
  }

  /** Feeds a pointer move (screen coordinates); returns whether hover may
   * select from now on. */
  move(screenX: number, screenY: number): boolean {
    if (this.moved) return true;
    if (this.origin === null) {
      this.origin = { x: screenX, y: screenY };
      return false;
    }
    this.moved = screenX !== this.origin.x || screenY !== this.origin.y;
    return this.moved;
  }

  get allowed(): boolean {
    return this.moved;
  }
}
