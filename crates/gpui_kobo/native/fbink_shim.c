#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "fbink.h"

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

int gpui_fbink_present_gray(
    int fd,
    unsigned char *data,
    int source_width,
    int source_height,
    size_t length,
    int target_x,
    int target_y,
    int target_width,
    int target_height,
    int refresh_mode
) {
    FBInkConfig config;
    quiet_config(&config);
    config.scaled_width = (short int) target_width;
    config.scaled_height = (short int) target_height;
    bool is_monochrome = refresh_mode == 0 || refresh_mode == 3 || refresh_mode == 4;
    bool is_full = refresh_mode == 2 || refresh_mode == 4;
    if (refresh_mode == 0) {
        config.wfm_mode = WFM_A2;
    } else if (refresh_mode == 3) {
        config.wfm_mode = WFM_DU;
    } else {
        config.wfm_mode = WFM_GC16;
    }
    config.dithering_mode = is_monochrome ? HWD_QUANT_ONLY : HWD_PASSTHROUGH;
    config.is_flashing = is_full;
    config.is_cleared = is_full;
    int result = fbink_print_raw_data(
        fd,
        data,
        source_width,
        source_height,
        length,
        (short int) target_x,
        (short int) target_y,
        &config
    );
    if (result >= 0 && is_full) {
        int wait_result = fbink_wait_for_complete(fd, LAST_MARKER);
        if (wait_result < 0) {
            return wait_result;
        }
    }
    return result;
}

int gpui_fbink_close(int fd) {
    return fbink_close(fd);
}
