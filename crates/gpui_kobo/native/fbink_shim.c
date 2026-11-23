#include <errno.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "fbink.h"

enum { GPUI_FBINK_NEEDS_PACKED_ROWS = 1 };

typedef struct {
    uint32_t view_width;
    uint32_t view_height;
    uint32_t screen_width;
    uint32_t screen_height;
    uint32_t view_x;
    uint32_t view_y;
    uint8_t current_rotation;
    uint8_t can_rotate;
    uint8_t is_sunxi;
    uint8_t reserved;
    char device_name[16];
    char device_codename[16];
    char device_platform[16];
} GpuiFbInkState;

static void quiet_config(FBInkConfig *config) {
    memset(config, 0, sizeof(*config));
    config->is_quiet = true;
    config->ignore_alpha = true;
    config->dithering_mode = HWD_PASSTHROUGH;
}

int gpui_fbink_set_rotation(int fd, uint8_t rota) {
    FBInkConfig config;
    quiet_config(&config);
    return fbink_set_fb_info(fd, fbink_rota_canonical_to_native(rota), KEEP_CURRENT_BITDEPTH, KEEP_CURRENT_GRAYSCALE, &config);
}

static void copy_state(GpuiFbInkState *output, const FBInkConfig *config) {
    FBInkState state;
    memset(&state, 0, sizeof(state));
    memset(output, 0, sizeof(*output));
    fbink_get_state(config, &state);
    output->view_width = state.view_width;
    output->view_height = state.view_height;
    output->screen_width = state.screen_width;
    output->screen_height = state.screen_height;
    output->view_x = state.view_hori_origin;
    output->view_y = state.view_vert_origin;
    output->current_rotation = fbink_rota_native_to_canonical(state.current_rota);
    output->can_rotate = state.can_rotate;
    output->is_sunxi = state.is_sunxi;
    snprintf(output->device_name, sizeof(output->device_name), "%s", state.device_name);
    snprintf(output->device_codename, sizeof(output->device_codename), "%s", state.device_codename);
    snprintf(output->device_platform, sizeof(output->device_platform), "%s", state.device_platform);
}

int gpui_fbink_open(GpuiFbInkState *state) {
    FBInkConfig config;
    quiet_config(&config);
    int fd = fbink_open();
    if (fd < 0) {
        return fd;
    }
    int result = fbink_init(fd, &config);
    if (result < 0) {
        fbink_close(fd);
        return result;
    }
    copy_state(state, &config);
    return fd;
}

int gpui_fbink_reinit(int fd, GpuiFbInkState *state) {
    FBInkConfig config;
    quiet_config(&config);
    int result = fbink_reinit(fd, &config);
    if (result < 0) {
        return result;
    }
    copy_state(state, &config);
    return result;
}

static bool write_gray32(
    int fd,
    const unsigned char *data,
    int width,
    int height,
    size_t source_stride,
    int target_x,
    int target_y,
    const FBInkConfig *config,
    FBInkRect *region
) {
    if (data == NULL || width <= 0 || height <= 0 || width > SHRT_MAX || height > SHRT_MAX ||
        target_x < 0 || target_y < 0 || target_x > SHRT_MAX || target_y > SHRT_MAX) {
        return false;
    }

    FBInkState state;
    fbink_get_state(config, &state);
    if (state.bpp != 32 || state.is_sunxi || state.is_ntx_quirky_landscape ||
        (state.pixel_format != FBINK_PXFMT_BGRA && state.pixel_format != FBINK_PXFMT_RGBA &&
         state.pixel_format != FBINK_PXFMT_BGR32 && state.pixel_format != FBINK_PXFMT_RGB32)) {
        return false;
    }

    struct fb_var_screeninfo variable_info;
    struct fb_fix_screeninfo fixed_info;
    fbink_get_fb_info(&variable_info, &fixed_info);
    if (variable_info.xoffset != 0 || variable_info.yoffset != 0 ||
        fixed_info.type != FB_TYPE_PACKED_PIXELS) {
        return false;
    }

    const int left = target_x + state.view_hori_origin;
    // Match FBInk's image placement: row zero ignores the text baseline offset.
    const int top = target_y + state.view_vert_origin - state.view_vert_offset;
    if (left > SHRT_MAX || top < 0 || top > SHRT_MAX ||
        (uint32_t) left + (uint32_t) width > state.screen_width ||
        (uint32_t) top + (uint32_t) height > state.screen_height ||
        state.scanline_stride % sizeof(uint32_t) != 0 ||
        ((uint64_t) left + (uint32_t) width) * sizeof(uint32_t) > state.scanline_stride) {
        return false;
    }

    // A complete overwrite makes the preliminary clear redundant.
    if (config->is_cleared && (left != 0 || top != 0 ||
        (uint32_t) width != state.screen_width || (uint32_t) height != state.screen_height)) {
        return false;
    }

    size_t buffer_size;
    unsigned char *framebuffer = fbink_get_fb_pointer(fd, &buffer_size);
    const uint64_t end_offset = ((uint64_t) top + (uint32_t) height - 1) * state.scanline_stride +
        ((uint64_t) left + (uint32_t) width) * sizeof(uint32_t);
    if (framebuffer == NULL || (uintptr_t) framebuffer % sizeof(uint32_t) != 0 || end_offset > buffer_size) {
        return false;
    }

    for (int row = 0; row < height; row++) {
        const unsigned char *source = data + (size_t) row * source_stride;
        uint32_t *destination = (uint32_t *) (framebuffer + (size_t) (top + row) * state.scanline_stride) + left;
        for (int column = 0; column < width; column++) {
            // Grayscale has identical RGB/BGR channels; the fourth byte is opaque.
            destination[column] = 0xFF000000U | (uint32_t) source[column] * 0x010101U;
        }
    }

    region->left = (unsigned short int) left;
    region->top = (unsigned short int) top;
    region->width = (unsigned short int) width;
    region->height = (unsigned short int) height;
    return true;
}

int gpui_fbink_present_gray(
    int fd,
    const unsigned char *data,
    int source_width,
    int source_height,
    size_t source_stride,
    size_t length,
    int target_x,
    int target_y,
    int target_width,
    int target_height,
    int refresh_mode
) {
    if (data == NULL || source_width <= 0 || source_height <= 0 ||
        source_stride < (size_t) source_width ||
        (size_t) (source_height - 1) > (SIZE_MAX - (size_t) source_width) / source_stride ||
        length < (size_t) (source_height - 1) * source_stride + (size_t) source_width) {
        return -EINVAL;
    }
    FBInkConfig config;
    quiet_config(&config);
    // FBInk treats nonzero dimensions as a rescale request, even at 1:1.
    if (source_width != target_width || source_height != target_height) {
        config.scaled_width = (short int) target_width;
        config.scaled_height = (short int) target_height;
    }
    bool is_monochrome = refresh_mode == 0 || refresh_mode == 3 || refresh_mode == 4 || refresh_mode == 5;
    bool is_full = refresh_mode == 2 || refresh_mode == 4;
    if (refresh_mode == 0) {
        config.wfm_mode = WFM_A2;
    } else if (refresh_mode == 3) {
        config.wfm_mode = WFM_DU;
    } else {
        config.wfm_mode = WFM_GC16;
    }
    config.dithering_mode = is_monochrome ? HWD_QUANT_ONLY : HWD_PASSTHROUGH;
    config.is_flashing = is_full || refresh_mode == 5 || refresh_mode == 6;
    config.is_cleared = is_full;
    int result;
    FBInkRect region;
    if (source_width == target_width && source_height == target_height &&
        write_gray32(fd, data, source_width, source_height, source_stride, target_x, target_y, &config, &region)) {
        result = fbink_refresh_rect(fd, &region, &config);
    } else {
        // FBInk's fallback accepts packed rows only. Ask Rust to pack its reused buffer.
        if (source_height > 1 && source_stride != (size_t) source_width) {
            return GPUI_FBINK_NEEDS_PACKED_ROWS;
        }
        result = fbink_print_raw_data(
            fd,
            data,
            source_width,
            source_height,
            (size_t) source_width * (size_t) source_height,
            (short int) target_x,
            (short int) target_y,
            &config
        );
    }
    if (result >= 0 && config.is_flashing) {
        int wait_result = fbink_wait_for_complete(fd, LAST_MARKER);
        if (wait_result < 0) {
            return wait_result;
        }
    }
    return result < 0 ? result : 0;
}

int gpui_fbink_close(int fd) {
    return fbink_close(fd);
}
