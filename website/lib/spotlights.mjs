const surfacesSelector =
  '.hero, .community-hero, .feature-card, .person-card, .contributor-profile, .upstream-card';
const preference =
  '(hover: hover) and (pointer: fine) and (prefers-reduced-motion: no-preference)';
const properties = ['--spotlight-x', '--spotlight-y', '--spotlight-opacity'];

export function installSpotlights({ root = document, view = window } = {}) {
  const media = view.matchMedia(preference);
  let detach = () => {};

  function configure() {
    detach();
    detach = () => {};
    if (!media.matches) return;

    const surfaces = Array.from(root.querySelectorAll(surfacesSelector));
    let pointer = null;
    let frame = null;

    function reset() {
      if (frame !== null) view.cancelAnimationFrame(frame);
      frame = null;
      pointer = null;
      for (const surface of surfaces) {
        for (const property of properties)
          surface.style.removeProperty(property);
      }
    }

    function paint() {
      frame = null;
      if (!pointer) return;
      // Read geometry together before writing paint-only custom properties.
      const positions = surfaces.map((surface) => {
        const rect = surface.getBoundingClientRect();
        const dx = Math.max(rect.left - pointer.x, 0, pointer.x - rect.right);
        const dy = Math.max(rect.top - pointer.y, 0, pointer.y - rect.bottom);
        const strength =
          rect.width && rect.height
            ? Math.max(0, 1 - Math.hypot(dx, dy) / 96)
            : 0;
        return {
          surface,
          x: pointer.x - rect.left,
          y: pointer.y - rect.top,
          strength,
        };
      });
      for (const { surface, x, y, strength } of positions) {
        if (strength > 0) {
          surface.style.setProperty('--spotlight-x', `${x.toFixed(1)}px`);
          surface.style.setProperty('--spotlight-y', `${y.toFixed(1)}px`);
        }
        surface.style.setProperty('--spotlight-opacity', strength.toFixed(3));
      }
    }

    function schedule() {
      if (pointer && frame === null) frame = view.requestAnimationFrame(paint);
    }
    function move(event) {
      if (event.pointerType === 'touch') {
        reset();
        return;
      }
      pointer = { x: event.clientX, y: event.clientY };
      schedule();
    }
    function leave(event) {
      if (event.relatedTarget === null) reset();
    }

    view.addEventListener('pointermove', move, { passive: true });
    view.addEventListener('pointerout', leave);
    view.addEventListener('blur', reset);
    view.addEventListener('scroll', schedule, { passive: true, capture: true });
    view.addEventListener('resize', schedule, { passive: true });
    detach = () => {
      view.removeEventListener('pointermove', move);
      view.removeEventListener('pointerout', leave);
      view.removeEventListener('blur', reset);
      view.removeEventListener('scroll', schedule, true);
      view.removeEventListener('resize', schedule);
      reset();
    };
  }

  configure();
  media.addEventListener('change', configure);
  return () => {
    media.removeEventListener('change', configure);
    detach();
  };
}
