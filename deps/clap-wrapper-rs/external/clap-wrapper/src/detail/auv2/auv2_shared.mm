
#include <clap/ext/gui.h>
#include <cstdint>
#include <iostream>

#include <Cocoa/Cocoa.h>

namespace free_audio::auv2_wrapper
{
bool auv2shared_mm_request_resize(const clap_window_t* win, uint32_t w, uint32_t h)
{
  if (!win) return false;

  auto* nsv = (NSView*)win;
  // ConjureWidth patch (see deps/PATCHES.md): report success, or the editor never resizes its own
  // contents and never records the new size. Keep the origin the host chose.
  NSRect frame = nsv.frame;
  if (frame.size.width != w || frame.size.height != h)
  {
    [nsv setFrame:NSMakeRect(frame.origin.x, frame.origin.y, w, h)];
  }

  return true;
}
}  // namespace free_audio::auv2_wrapper
