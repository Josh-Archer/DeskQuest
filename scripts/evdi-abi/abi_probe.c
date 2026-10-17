/*
 * Prints the sizes, offsets and ioctl request numbers of the EVDI userspace ABI that
 * crates/display/src/backend/kernel_drm/abi.rs hard-codes, as computed by a C compiler from the
 * real headers. The output is checked in as crates/display/tests/fixtures/evdi-<pin>-abi.txt
 * and the Rust unit test `abi_matches_c_probe_fixture` compares every constant to it.
 *
 * Regenerate with scripts/evdi-abi/regen.sh. Build inputs:
 *   - evdi_drm.h from DisplayLink/evdi (module/evdi_drm.h) at a pinned commit,
 *   - drm/drm.h and the asm ioctl headers from the linux-libc-dev package.
 * The probe never executes anything from those directories.
 */
#include <stddef.h>
#include <stdio.h>

#include <drm/drm.h>

/* evdi_drm.h annotates userspace pointers with __user; it is empty outside the kernel. */
#ifndef __user
#define __user
#endif
#include "evdi_drm.h"

#define SIZE(t) printf("sizeof(%s) = %zu\n", #t, sizeof(t))
#define OFF(t, f) printf("offsetof(%s, %s) = %zu\n", #t, #f, offsetof(t, f))
#define CONST_HEX(c) printf("%s = 0x%08lx\n", #c, (unsigned long)(c))
#define CONST_DEC(c) printf("%s = %lu\n", #c, (unsigned long)(c))

int main(void)
{
	printf("sizeof(void*) = %zu\n", sizeof(void *));

	/* Core DRM structs/ioctls we use beside the EVDI ones. */
	SIZE(struct drm_event);
	OFF(struct drm_event, type);
	OFF(struct drm_event, length);
	SIZE(struct drm_clip_rect);
	OFF(struct drm_clip_rect, x1);
	OFF(struct drm_clip_rect, y1);
	OFF(struct drm_clip_rect, x2);
	OFF(struct drm_clip_rect, y2);
	SIZE(struct drm_version);
	OFF(struct drm_version, version_major);
	OFF(struct drm_version, version_minor);
	OFF(struct drm_version, version_patchlevel);
	OFF(struct drm_version, name_len);
	OFF(struct drm_version, name);
	OFF(struct drm_version, date_len);
	OFF(struct drm_version, date);
	OFF(struct drm_version, desc_len);
	OFF(struct drm_version, desc);
	CONST_HEX(DRM_IOCTL_VERSION);
	CONST_HEX(DRM_IOCTL_DROP_MASTER);
	CONST_DEC(DRM_COMMAND_BASE);

	/* EVDI ioctl payloads. */
	SIZE(struct drm_evdi_connect);
	OFF(struct drm_evdi_connect, connected);
	OFF(struct drm_evdi_connect, dev_index);
	OFF(struct drm_evdi_connect, edid);
	OFF(struct drm_evdi_connect, edid_length);
	OFF(struct drm_evdi_connect, pixel_area_limit);
	OFF(struct drm_evdi_connect, pixel_per_second_limit);

	SIZE(struct drm_evdi_request_update);
	OFF(struct drm_evdi_request_update, reserved);

	SIZE(struct drm_evdi_grabpix);
	OFF(struct drm_evdi_grabpix, mode);
	OFF(struct drm_evdi_grabpix, buf_width);
	OFF(struct drm_evdi_grabpix, buf_height);
	OFF(struct drm_evdi_grabpix, buf_byte_stride);
	OFF(struct drm_evdi_grabpix, buffer);
	OFF(struct drm_evdi_grabpix, num_rects);
	OFF(struct drm_evdi_grabpix, rects);
	CONST_DEC(EVDI_GRABPIX_MODE_RECTS);
	CONST_DEC(EVDI_GRABPIX_MODE_DIRTY);

	/* EVDI events read() from the card fd. */
	SIZE(struct drm_evdi_event_update_ready);
	SIZE(struct drm_evdi_event_dpms);
	OFF(struct drm_evdi_event_dpms, mode);
	SIZE(struct drm_evdi_event_mode_changed);
	OFF(struct drm_evdi_event_mode_changed, hdisplay);
	OFF(struct drm_evdi_event_mode_changed, vdisplay);
	OFF(struct drm_evdi_event_mode_changed, vrefresh);
	OFF(struct drm_evdi_event_mode_changed, bits_per_pixel);
	OFF(struct drm_evdi_event_mode_changed, pixel_format);
	SIZE(struct drm_evdi_event_crtc_state);
	OFF(struct drm_evdi_event_crtc_state, state);

	CONST_HEX(DRM_EVDI_EVENT_UPDATE_READY);
	CONST_HEX(DRM_EVDI_EVENT_DPMS);
	CONST_HEX(DRM_EVDI_EVENT_MODE_CHANGED);
	CONST_HEX(DRM_EVDI_EVENT_CRTC_STATE);
	CONST_HEX(DRM_EVDI_EVENT_CURSOR_SET);
	CONST_HEX(DRM_EVDI_EVENT_CURSOR_MOVE);
	CONST_HEX(DRM_EVDI_EVENT_DDCCI_DATA);

	/* Command indices and full request numbers. */
	CONST_HEX(DRM_EVDI_CONNECT);
	CONST_HEX(DRM_EVDI_REQUEST_UPDATE);
	CONST_HEX(DRM_EVDI_GRABPIX);
	CONST_HEX(DRM_IOCTL_EVDI_CONNECT);
	CONST_HEX(DRM_IOCTL_EVDI_REQUEST_UPDATE);
	CONST_HEX(DRM_IOCTL_EVDI_GRABPIX);
	return 0;
}
