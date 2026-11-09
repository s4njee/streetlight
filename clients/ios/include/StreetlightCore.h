// C ABI of crates/client-core (src/ffi.rs). Strings returned by sl_* must be freed
// with sl_string_free. Error out-params (char **err) may be NULL.
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct SLClient SLClient;

typedef void (*sl_video_cb)(void *ctx, const uint8_t *data, size_t len, uint32_t frame, bool keyframe, uint64_t received_us);
typedef void (*sl_audio_cb)(void *ctx, const uint8_t *data, size_t len);

void sl_string_free(char *s);

SLClient *sl_connect(const char *host, const char *client_name, const char *data_dir, const char *caps_json,
                     char **welcome_json, char **err);
void sl_free(SLClient *c);
char *sl_random_pin(void);
int32_t sl_pair(const SLClient *c, const char *pin, char **err);
char *sl_apps_json(const SLClient *c, char **err);
char *sl_start(const SLClient *c, const char *request_json, sl_video_cb video, sl_audio_cb audio, void *ctx, char **err);
void sl_stop(const SLClient *c);
void sl_request_idr(const SLClient *c);
void sl_report_timing(const SLClient *c, uint32_t decode_us, uint32_t present_us);
char *sl_stats_json(const SLClient *c);
char *sl_stopped_reason(const SLClient *c);
char *sl_known_hosts_json(const char *data_dir);
void sl_forget_host(const char *data_dir, const char *fingerprint);
char *sl_discover_json(uint32_t timeout_ms);

void sl_input_key(const SLClient *c, uint16_t evdev_code, bool down);
void sl_input_mouse_move(const SLClient *c, int32_t dx, int32_t dy);
void sl_input_mouse_abs(const SLClient *c, uint16_t x, uint16_t y);
void sl_input_mouse_button(const SLClient *c, uint8_t button, bool down);
void sl_input_scroll(const SLClient *c, int32_t dx, int32_t dy);
void sl_input_touch(const SLClient *c, uint8_t id, uint8_t phase, uint16_t x, uint16_t y);
void sl_input_gamepad(const SLClient *c, uint8_t id, uint32_t buttons, int16_t lx, int16_t ly, int16_t rx, int16_t ry,
                      uint8_t lt, uint8_t rt);
void sl_input_gamepad_disconnected(const SLClient *c, uint8_t id);
void sl_input_text(const SLClient *c, const char *utf8);
