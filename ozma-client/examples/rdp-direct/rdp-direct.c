/* SPDX-License-Identifier: AGPL-3.0-only */
/*
 * Direct-surface outbound RDP bridge.
 *
 * Unlike rdp_output's Xvfb/ffplay shadow prototype, this program owns a
 * FreeRDP shadow *subsystem* backed by an in-memory surface.  It reads OZFR
 * records on stdin, writes the pixels straight into the shadow server's
 * primary surface, marks the changed region, and publishes a frame to the
 * connected viewers.  There is no X server, no ffplay and no intermediate
 * composed-frame display.
 *
 * OZFR records are:
 *   magic[4] = "OZFR", width/height/stride as big-endian u32, RGBA8 rows.
 *
 * Security and policy:
 *   - TLS-only (NLA, EXT and legacy RDP security are disabled).
 *   - A viewer credential file or a freshly generated token is required.
 *   - Binds loopback by default; a non-loopback bind prints a warning.
 *   - Concurrent authorized viewers are limited (default 1).
 *   - Sessions are view-only; input callbacks are deliberately ignored.
 *
 * This is a development bridge.  It does not implement input, audio,
 * clipboard, dynamic resize, or its own RDPEGFX/H.264 encoder.  FreeRDP's
 * shadow server can negotiate its library codecs only when explicitly built
 * and enabled; this program leaves them off and uses bitmap updates.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <freerdp/server/shadow.h>

#include <winpr/collections.h>
#include <winpr/crt.h>
#include <winpr/synch.h>

#define OZFR_HEADER_LEN 16u
#define OZFR_MAX_DIMENSION 8192u
#define OZFR_MAX_FRAME_BYTES (64u * 1024u * 1024u)
#define BRIDGE_MAX_CREDENTIALS 8u
#define BRIDGE_TOKEN_BYTES 32u
#define BRIDGE_DEFAULT_PORT 3390
#define BRIDGE_DEFAULT_TOKEN_FILE "/tmp/ozma-rdp-viewer-token"

typedef struct
{
	char* user;
	char* secret;
} BridgeCredential;

typedef struct
{
	int fd;
	uint32_t width;
	uint32_t height;
	uint32_t stride;
	uint8_t* pixels;
} OzfrFrame;

typedef struct
{
	uint32_t port;
	char* bind_address;
	char* config_dir;
	char* token_file;
	char* auth_file;
	char* token_user;
	uint32_t max_viewers;
	bool help;
} BridgeOptions;

typedef struct
{
	CRITICAL_SECTION lock;
	CRITICAL_SECTION publish_lock;
	BridgeCredential credentials[BRIDGE_MAX_CREDENTIALS];
	size_t credential_count;
	uint32_t max_viewers;
	uint32_t width;
	uint32_t height;
	uint64_t frames_published;
	uint64_t viewers_allowed;
	uint64_t viewers_rejected;
	rdpShadowServer* server;
} BridgeState;

static volatile sig_atomic_t g_stop = 0;
static BridgeState g_state;

static void on_signal(int signal_number)
{
	(void)signal_number;
	g_stop = 1;
}

static void bridge_log(const char* format, ...)
{
	va_list args;
	va_start(args, format);
	fputs("rdp-direct: ", stderr);
	vfprintf(stderr, format, args);
	fputc('\n', stderr);
	fflush(stderr);
	va_end(args);
}

static void usage(FILE* out, const char* argv0)
{
	fprintf(out,
	        "Usage: %s [options] < scene.ozfr\n"
	        "\n"
	        "Direct FreeRDP shadow surface bridge for the OZFR frame stream.\n"
	        "\n"
	        "  --port N              RDP listener port (default %d)\n"
	        "  --bind ADDR           bind address (default 127.0.0.1)\n"
	        "  --auth-file PATH      credential file: 'user:secret' or a bare token\n"
	        "  --token-file PATH     generate a token here (default %s)\n"
	        "  --user NAME           username for a generated token (default ozma)\n"
	        "  --max-viewers N       concurrent authorized viewers, 0 = unlimited (default 1)\n"
	        "  --config-dir PATH     FreeRDP certificate/config directory\n"
	        "  --help                show this help\n"
	        "\n"
	        "TLS is always required.  The listener is view-only.\n",
	        argv0, BRIDGE_DEFAULT_PORT, BRIDGE_DEFAULT_TOKEN_FILE);
}

static bool parse_u32(const char* text, uint32_t min, uint32_t max, uint32_t* out)
{
	char* end = NULL;
	unsigned long value = 0;

	if (!text || !*text)
		return false;

	errno = 0;
	value = strtoul(text, &end, 10);
	if (errno != 0 || !end || *end != '\0' || value < min || value > max)
		return false;

	*out = (uint32_t)value;
	return true;
}

static bool parse_options(int argc, char** argv, BridgeOptions* options)
{
	int option = 0;
	uint32_t value = 0;
	static const struct option long_options[] = {
		{ "port", required_argument, NULL, 'p' },
		{ "bind", required_argument, NULL, 'b' },
		{ "auth-file", required_argument, NULL, 'a' },
		{ "token-file", required_argument, NULL, 't' },
		{ "user", required_argument, NULL, 'u' },
		{ "max-viewers", required_argument, NULL, 'm' },
		{ "config-dir", required_argument, NULL, 'c' },
		{ "help", no_argument, NULL, 'h' },
		{ NULL, 0, NULL, 0 },
	};

	memset(options, 0, sizeof(*options));
	options->port = BRIDGE_DEFAULT_PORT;
	options->max_viewers = 1;
	options->token_user = strdup("ozma");
	options->token_file = strdup(BRIDGE_DEFAULT_TOKEN_FILE);
	options->bind_address = strdup("127.0.0.1");
	if (!options->token_user || !options->token_file || !options->bind_address)
		return false;

	opterr = 0;
	while ((option = getopt_long(argc, argv, "p:b:a:t:u:m:c:h", long_options, NULL)) != -1)
	{
		switch (option)
		{
			case 'p':
				if (!parse_u32(optarg, 1, 65535, &value))
				{
					bridge_log("invalid --port '%s'", optarg ? optarg : "");
					return false;
				}
				options->port = value;
				break;
			case 'b':
				free(options->bind_address);
				options->bind_address = strdup(optarg);
				if (!options->bind_address)
					return false;
				break;
			case 'a':
				free(options->auth_file);
				options->auth_file = strdup(optarg);
				if (!options->auth_file)
					return false;
				break;
			case 't':
				free(options->token_file);
				options->token_file = strdup(optarg);
				if (!options->token_file)
					return false;
				break;
			case 'u':
				free(options->token_user);
				options->token_user = strdup(optarg);
				if (!options->token_user)
					return false;
				break;
			case 'm':
				if (!parse_u32(optarg, 0, 1024, &value))
				{
					bridge_log("invalid --max-viewers '%s'", optarg ? optarg : "");
					return false;
				}
				options->max_viewers = value;
				break;
			case 'c':
				free(options->config_dir);
				options->config_dir = strdup(optarg);
				if (!options->config_dir)
					return false;
				break;
			case 'h':
				options->help = true;
				break;
			default:
				bridge_log("unknown option; use --help");
				return false;
		}
	}

	if (optind != argc)
	{
		bridge_log("unexpected argument '%s'; OZFR is read from stdin", argv[optind]);
		return false;
	}

	return true;
}

static void free_options(BridgeOptions* options)
{
	free(options->bind_address);
	free(options->config_dir);
	free(options->token_file);
	free(options->auth_file);
	free(options->token_user);
	memset(options, 0, sizeof(*options));
}

static bool read_exact(int fd, void* buffer, size_t length)
{
	uint8_t* cursor = (uint8_t*)buffer;
	size_t done = 0;

	while (done < length)
	{
		ssize_t got = read(fd, cursor + done, length - done);
		if (got == 0)
			return false;
		if (got < 0)
		{
			if (errno == EINTR)
				continue;
			return false;
		}
		done += (size_t)got;
	}
	return true;
}

static uint32_t read_be32(const uint8_t* bytes)
{
	return ((uint32_t)bytes[0] << 24) | ((uint32_t)bytes[1] << 16) | ((uint32_t)bytes[2] << 8) |
	       (uint32_t)bytes[3];
}

static void free_frame(OzfrFrame* frame)
{
	free(frame->pixels);
	memset(frame, 0, sizeof(*frame));
}

/* Reads one OZFR frame.  EOF between frames returns 1 with fd set to -1. */
static int read_frame(OzfrFrame* frame, bool allow_eof)
{
	uint8_t header[OZFR_HEADER_LEN];
	uint64_t bytes = 0;

	memset(frame, 0, sizeof(*frame));
	frame->fd = -1;

	/* A partial header is an error; a clean EOF before any byte is not. */
	{
		uint8_t first = 0;
		for (;;)
		{
			ssize_t got = read(STDIN_FILENO, &first, 1);
			if (got == 1)
				break;
			if (got == 0)
			{
				if (allow_eof)
					return 1;
				bridge_log("truncated OZFR header");
				return -1;
			}
			if (errno == EINTR)
			{
				if (g_stop)
					return 1;
				continue;
			}
			bridge_log("reading OZFR stream: %s", strerror(errno));
			return -1;
		}
		header[0] = first;
	}

	if (!read_exact(STDIN_FILENO, &header[1], sizeof(header) - 1))
	{
		bridge_log("truncated OZFR header");
		return -1;
	}

	if (memcmp(header, "OZFR", 4) != 0)
	{
		bridge_log("invalid OZFR magic");
		return -1;
	}

	frame->width = read_be32(&header[4]);
	frame->height = read_be32(&header[8]);
	frame->stride = read_be32(&header[12]);

	if (frame->width == 0 || frame->height == 0 || frame->width > OZFR_MAX_DIMENSION ||
	    frame->height > OZFR_MAX_DIMENSION || frame->stride < frame->width * 4u)
	{
		bridge_log("invalid OZFR layout %ux%u stride %u", frame->width, frame->height,
		           frame->stride);
		return -1;
	}

	bytes = (uint64_t)frame->stride * frame->height;
	if (bytes == 0 || bytes > OZFR_MAX_FRAME_BYTES)
	{
		bridge_log("OZFR frame too large (%llu bytes)", (unsigned long long)bytes);
		return -1;
	}

	frame->pixels = (uint8_t*)malloc((size_t)bytes);
	if (!frame->pixels)
	{
		bridge_log("out of memory for %llu-byte frame", (unsigned long long)bytes);
		return -1;
	}

	if (!read_exact(STDIN_FILENO, frame->pixels, (size_t)bytes))
	{
		bridge_log("truncated OZFR frame");
		free_frame(frame);
		return -1;
	}

	frame->fd = STDIN_FILENO;
	return 0;
}

static bool constant_time_equal(const char* left, const char* right)
{
	size_t left_length = strlen(left);
	size_t right_length = strlen(right);
	size_t length = left_length > right_length ? left_length : right_length;
	unsigned char difference = (left_length == right_length) ? 0u : 1u;

	for (size_t index = 0; index < length; index++)
	{
		const unsigned char a = index < left_length ? (unsigned char)left[index] : 0u;
		const unsigned char b = index < right_length ? (unsigned char)right[index] : 0u;
		difference |= (unsigned char)(a ^ b);
	}

	return difference == 0u;
}

static void free_credentials(BridgeState* state)
{
	for (size_t index = 0; index < state->credential_count; index++)
	{
		free(state->credentials[index].user);
		free(state->credentials[index].secret);
	}
	memset(state->credentials, 0, sizeof(state->credentials));
	state->credential_count = 0;
}

static bool add_credential(BridgeState* state, const char* user, const char* secret)
{
	BridgeCredential* credential = NULL;

	if (state->credential_count >= BRIDGE_MAX_CREDENTIALS)
		return false;
	if (!user || !*user || !secret || !*secret)
		return false;

	credential = &state->credentials[state->credential_count];
	credential->user = strdup(user);
	credential->secret = strdup(secret);
	if (!credential->user || !credential->secret)
	{
		free(credential->user);
		free(credential->secret);
		memset(credential, 0, sizeof(*credential));
		return false;
	}

	state->credential_count++;
	return true;
}

static char* trim(char* text)
{
	char* end = NULL;

	while (*text == ' ' || *text == '\t' || *text == '\r' || *text == '\n')
		text++;
	end = text + strlen(text);
	while (end > text && (end[-1] == ' ' || end[-1] == '\t' || end[-1] == '\r' || end[-1] == '\n'))
		end--;
	*end = '\0';
	return text;
}

static bool load_auth_file(BridgeState* state, const char* path)
{
	char buffer[4096];
	ssize_t length = 0;
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	char* line = NULL;
	char* save = NULL;
	size_t loaded = 0;

	if (fd < 0)
	{
		bridge_log("cannot open --auth-file '%s': %s", path, strerror(errno));
		return false;
	}

	length = read(fd, buffer, sizeof(buffer) - 1);
	close(fd);
	if (length < 0)
	{
		bridge_log("cannot read --auth-file '%s': %s", path, strerror(errno));
		return false;
	}
	buffer[length] = '\0';

	for (line = strtok_r(buffer, "\n", &save); line; line = strtok_r(NULL, "\n", &save))
	{
		char* text = trim(line);
		char* separator = NULL;

		if (!*text || *text == '#')
			continue;

		separator = strchr(text, ':');
		if (separator)
		{
			*separator = '\0';
			if (!add_credential(state, trim(text), trim(separator + 1)))
				goto fail;
		}
		else if (!add_credential(state, "ozma", text))
		{
			goto fail;
		}
		loaded++;
	}

	if (loaded == 0)
	{
		bridge_log("--auth-file '%s' contains no credentials", path);
		return false;
	}

	bridge_log("loaded %zu credential entr%s from %s", loaded, loaded == 1 ? "y" : "ies", path);
	return true;

fail:
	bridge_log("--auth-file '%s' has too many or invalid entries (max %u)", path,
	           BRIDGE_MAX_CREDENTIALS);
	return false;
}

static bool fill_random(uint8_t* buffer, size_t length)
{
	size_t done = 0;

	while (done < length)
	{
		ssize_t got = getrandom(buffer + done, length - done, 0);
		if (got < 0)
		{
			if (errno == EINTR)
				continue;
			break;
		}
		if (got == 0)
			break;
		done += (size_t)got;
	}

	if (done == length)
		return true;

	{
		int fd = open("/dev/urandom", O_RDONLY | O_CLOEXEC);
		if (fd < 0)
			return false;
		while (done < length)
		{
			ssize_t got = read(fd, buffer + done, length - done);
			if (got <= 0)
			{
				if (got < 0 && errno == EINTR)
					continue;
				close(fd);
				return false;
			}
			done += (size_t)got;
		}
		close(fd);
	}

	return true;
}

static bool write_token_file(const char* path, const char* token)
{
	char temporary[4096];
	int fd = -1;
	size_t length = strlen(token);
	ssize_t written = 0;

	if (snprintf(temporary, sizeof(temporary), "%s.tmp.%ld", path, (long)getpid()) >=
	    (int)sizeof(temporary))
	{
		bridge_log("token path is too long");
		return false;
	}

	fd = open(temporary, O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW | O_CLOEXEC, 0600);
	if (fd < 0)
	{
		bridge_log("cannot create token file '%s': %s", temporary, strerror(errno));
		return false;
	}
	if (fchmod(fd, 0600) != 0)
	{
		bridge_log("cannot restrict token file permissions: %s", strerror(errno));
		close(fd);
		unlink(temporary);
		return false;
	}
	while ((size_t)written < length)
	{
		ssize_t got = write(fd, token + written, length - (size_t)written);
		if (got < 0)
		{
			if (errno == EINTR)
				continue;
			bridge_log("cannot write token file: %s", strerror(errno));
			close(fd);
			unlink(temporary);
			return false;
		}
		if (got == 0)
		{
			bridge_log("cannot write token file: short write");
			close(fd);
			unlink(temporary);
			return false;
		}
		written += got;
	}
	if (write(fd, "\n", 1) != 1)
	{
		bridge_log("cannot finish token file: %s", strerror(errno));
		close(fd);
		unlink(temporary);
		return false;
	}
	if (close(fd) != 0)
	{
		bridge_log("cannot close token file: %s", strerror(errno));
		unlink(temporary);
		return false;
	}
	if (rename(temporary, path) != 0)
	{
		bridge_log("cannot publish token file '%s': %s", path, strerror(errno));
		unlink(temporary);
		return false;
	}

	return true;
}

static bool generate_token(BridgeState* state, const BridgeOptions* options, char* token,
                           size_t token_size)
{
	static const char hex[] = "0123456789abcdef";
	uint8_t random[BRIDGE_TOKEN_BYTES];
	size_t needed = BRIDGE_TOKEN_BYTES * 2 + 1;

	if (token_size < needed)
		return false;
	if (!fill_random(random, sizeof(random)))
	{
		bridge_log("cannot obtain random bytes for viewer token");
		return false;
	}
	for (size_t index = 0; index < BRIDGE_TOKEN_BYTES; index++)
	{
		token[index * 2] = hex[random[index] >> 4];
		token[index * 2 + 1] = hex[random[index] & 0x0f];
	}
	token[BRIDGE_TOKEN_BYTES * 2] = '\0';

	if (!add_credential(state, options->token_user, token))
	{
		bridge_log("cannot store generated viewer token");
		return false;
	}
	if (!write_token_file(options->token_file, token))
		return false;

	bridge_log("generated viewer token for user '%s' in %s (mode 0600)", options->token_user,
	           options->token_file);
	bridge_log("token: %s", token);
	return true;
}

static void log_rejection(const rdpShadowClient* client, const char* reason)
{
	const char* host = "unknown";
	if (client && client->context.peer && client->context.peer->hostname[0])
		host = client->context.peer->hostname;
	EnterCriticalSection(&g_state.lock);
	g_state.viewers_rejected++;
	LeaveCriticalSection(&g_state.lock);
	bridge_log("viewer rejected from %s: %s", host, reason);
}

static int bridge_authenticate(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                               const char* user, const char* domain, const char* password)
{
	const char* username = (user && *user) ? user : "";
	size_t connections = 0;

	(void)subsystem;
	(void)domain;

	if (!password || !*password)
	{
		log_rejection(client, "missing password or viewer token");
		return -1;
	}
	if (!*username)
	{
		log_rejection(client, "missing username");
		return -1;
	}

	connections = ArrayList_Count(g_state.server->clients);
	if (g_state.max_viewers > 0 && connections > g_state.max_viewers)
	{
		char reason[96];
		(void)snprintf(reason, sizeof(reason),
		               "viewer limit reached (max %u concurrent connection%s)",
		               g_state.max_viewers, g_state.max_viewers == 1 ? "" : "s");
		log_rejection(client, reason);
		return -1;
	}

	for (size_t index = 0; index < g_state.credential_count; index++)
	{
		const BridgeCredential* credential = &g_state.credentials[index];
		if (constant_time_equal(username, credential->user) &&
		    constant_time_equal(password, credential->secret))
		{
			EnterCriticalSection(&g_state.lock);
			g_state.viewers_allowed++;
			LeaveCriticalSection(&g_state.lock);
			return 1;
		}
	}

	log_rejection(client, "invalid username or viewer token");
	return -1;
}

static BOOL bridge_client_connect(rdpShadowSubsystem* subsystem, rdpShadowClient* client)
{
	const char* host = "unknown";

	(void)subsystem;

	if (client && client->context.peer && client->context.peer->hostname[0])
		host = client->context.peer->hostname;

	/* Enforce view-only per connection even if server defaults change. */
	client->mayView = TRUE;
	client->mayInteract = FALSE;

	bridge_log("viewer connected from %s (view-only)", host);
	return TRUE;
}

static void bridge_client_disconnect(rdpShadowSubsystem* subsystem, rdpShadowClient* client)
{
	const char* host = "unknown";

	(void)subsystem;

	if (client && client->context.peer && client->context.peer->hostname[0])
		host = client->context.peer->hostname;

	bridge_log("viewer disconnected from %s", host);
}

static BOOL ignore_synchronize_event(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                                     UINT32 flags)
{
	(void)subsystem;
	(void)client;
	(void)flags;
	return TRUE;
}

static BOOL ignore_keyboard_event(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                                  UINT16 flags, UINT8 code)
{
	(void)subsystem;
	(void)client;
	(void)flags;
	(void)code;
	return TRUE;
}

static BOOL ignore_unicode_keyboard_event(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                                          UINT16 flags, UINT16 code)
{
	(void)subsystem;
	(void)client;
	(void)flags;
	(void)code;
	return TRUE;
}

static BOOL ignore_mouse_event(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                               UINT16 flags, UINT16 x, UINT16 y)
{
	(void)subsystem;
	(void)client;
	(void)flags;
	(void)x;
	(void)y;
	return TRUE;
}

static BOOL ignore_rel_mouse_event(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                                   UINT16 flags, INT16 x_delta, INT16 y_delta)
{
	(void)subsystem;
	(void)client;
	(void)flags;
	(void)x_delta;
	(void)y_delta;
	return TRUE;
}

static BOOL ignore_extended_mouse_event(rdpShadowSubsystem* subsystem, rdpShadowClient* client,
                                        UINT16 flags, UINT16 x, UINT16 y)
{
	(void)subsystem;
	(void)client;
	(void)flags;
	(void)x;
	(void)y;
	return TRUE;
}

typedef struct
{
	rdpShadowSubsystem common;
	HANDLE thread;
} BridgeSubsystem;

static void bridge_subsystem_message_free(wMessage* message)
{
	if (message->Free)
		message->Free(message);
	message->Free = NULL;
}

static DWORD WINAPI bridge_subsystem_thread(LPVOID argument)
{
	rdpShadowSubsystem* subsystem = (rdpShadowSubsystem*)argument;
	wMessage message;

	WINPR_ASSERT(subsystem);

	for (;;)
	{
		if (WaitForSingleObject(MessageQueue_Event(subsystem->MsgPipe->In), INFINITE) !=
		    WAIT_OBJECT_0)
			break;

		while (MessageQueue_Peek(subsystem->MsgPipe->In, &message, TRUE))
		{
			if (message.id == WMQ_QUIT)
			{
				bridge_subsystem_message_free(&message);
				ExitThread(0);
				return 0;
			}

			if (message.id == SHADOW_MSG_IN_REFRESH_REQUEST_ID)
			{
				/* A newly activated viewer asks for a full frame.  Publish under
				 * the same lock as the stdin frame path so the surface and its
				 * invalid region are never observed mid-update. */
				EnterCriticalSection(&g_state.publish_lock);
				shadow_subsystem_frame_update(subsystem);
				LeaveCriticalSection(&g_state.publish_lock);
			}
			else
			{
				bridge_log("ignoring subsystem message %u", message.id);
			}

			bridge_subsystem_message_free(&message);
		}
	}

	ExitThread(0);
	return 0;
}

static rdpShadowSubsystem* bridge_subsystem_new(void)
{
	BridgeSubsystem* subsystem = (BridgeSubsystem*)calloc(1, sizeof(*subsystem));

	if (!subsystem)
		return NULL;

	subsystem->common.Authenticate = bridge_authenticate;
	subsystem->common.ClientConnect = bridge_client_connect;
	subsystem->common.ClientDisconnect = bridge_client_disconnect;
	subsystem->common.SynchronizeEvent = ignore_synchronize_event;
	subsystem->common.KeyboardEvent = ignore_keyboard_event;
	subsystem->common.UnicodeKeyboardEvent = ignore_unicode_keyboard_event;
	subsystem->common.MouseEvent = ignore_mouse_event;
	subsystem->common.RelMouseEvent = ignore_rel_mouse_event;
	subsystem->common.ExtendedMouseEvent = ignore_extended_mouse_event;
	return &subsystem->common;
}

static void bridge_subsystem_free(rdpShadowSubsystem* subsystem)
{
	free(subsystem);
}

static int bridge_subsystem_init(rdpShadowSubsystem* subsystem)
{
	MONITOR_DEF* monitor = NULL;
	MONITOR_DEF* virtual_screen = NULL;

	if (!subsystem || g_state.width == 0 || g_state.height == 0)
		return -1;

	subsystem->numMonitors = 1;
	memset(subsystem->monitors, 0, sizeof(subsystem->monitors));
	monitor = &subsystem->monitors[0];
	monitor->left = 0;
	monitor->top = 0;
	monitor->right = (INT32)g_state.width - 1;
	monitor->bottom = (INT32)g_state.height - 1;
	monitor->flags = 1;

	virtual_screen = &subsystem->virtualScreen;
	*virtual_screen = *monitor;
	subsystem->captureFrameRate = 30;

	/* No X11 fd event is needed; the frame source is stdin. */
	subsystem->event = NULL;
	return 1;
}

static int bridge_subsystem_uninit(rdpShadowSubsystem* subsystem)
{
	(void)subsystem;
	return 1;
}

static int bridge_subsystem_start(rdpShadowSubsystem* subsystem)
{
	BridgeSubsystem* bridge = (BridgeSubsystem*)subsystem;

	if (!bridge)
		return -1;

	bridge->thread = CreateThread(NULL, 0, bridge_subsystem_thread, subsystem, 0, NULL);
	if (!bridge->thread)
	{
		bridge_log("cannot start subsystem message thread");
		return -1;
	}

	return 1;
}

static int bridge_subsystem_stop(rdpShadowSubsystem* subsystem)
{
	BridgeSubsystem* bridge = (BridgeSubsystem*)subsystem;

	if (!bridge)
		return -1;

	if (bridge->thread)
	{
		if (MessageQueue_PostQuit(subsystem->MsgPipe->In, 0))
			(void)WaitForSingleObject(bridge->thread, INFINITE);
		(void)CloseHandle(bridge->thread);
		bridge->thread = NULL;
	}

	return 1;
}

static UINT32 bridge_enum_monitors(MONITOR_DEF* monitors, UINT32 max_monitors)
{
	if (!monitors || max_monitors < 1 || g_state.width == 0 || g_state.height == 0)
		return 0;

	monitors[0].left = 0;
	monitors[0].top = 0;
	monitors[0].right = (INT32)g_state.width - 1;
	monitors[0].bottom = (INT32)g_state.height - 1;
	monitors[0].flags = 1;
	return 1;
}

static int bridge_subsystem_entry(RDP_SHADOW_ENTRY_POINTS* entry_points)
{
	if (!entry_points)
		return -1;

	entry_points->New = bridge_subsystem_new;
	entry_points->Free = bridge_subsystem_free;
	entry_points->Init = bridge_subsystem_init;
	entry_points->Uninit = bridge_subsystem_uninit;
	entry_points->Start = bridge_subsystem_start;
	entry_points->Stop = bridge_subsystem_stop;
	entry_points->EnumMonitors = bridge_enum_monitors;
	return 1;
}

/*
 * Publish one RGBA frame into FreeRDP's primary surface.
 *
 * The shadow surface is BGRX32, so the conversion is R/G/B byte swap plus an
 * opaque alpha byte.  The entire frame is marked invalid.  FreeRDP's shadow
 * client then sends the changed region as bitmap updates (we disable its
 * optional GFX/RFX/NSC codec paths in configure_server).
 */
static bool publish_frame(const OzfrFrame* frame)
{
	rdpShadowSurface* surface = NULL;
	RECTANGLE_16 rect;
	uint32_t width = 0;
	uint32_t height = 0;

	if (!g_state.server || !g_state.server->surface)
	{
		bridge_log("no FreeRDP surface available");
		return false;
	}

	surface = g_state.server->surface;

	if (surface->width != frame->width || surface->height != frame->height)
	{
		bridge_log("OZFR dimensions changed from %ux%u to %ux%u; reconnect viewers",
		           surface->width, surface->height, frame->width, frame->height);
		return false;
	}

	width = surface->width;
	height = surface->height;
	if (frame->stride < width * 4u)
	{
		bridge_log("OZFR stride %u is too small for width %u", frame->stride, width);
		return false;
	}

	EnterCriticalSection(&g_state.publish_lock);

	EnterCriticalSection(&surface->lock);
	for (uint32_t y = 0; y < height; y++)
	{
		const uint8_t* source = frame->pixels + (size_t)y * frame->stride;
		uint8_t* destination = surface->data + (size_t)y * surface->scanline;

		for (uint32_t x = 0; x < width; x++)
		{
			destination[x * 4 + 0] = source[x * 4 + 2]; /* B */
			destination[x * 4 + 1] = source[x * 4 + 1]; /* G */
			destination[x * 4 + 2] = source[x * 4 + 0]; /* R */
			destination[x * 4 + 3] = 0xff;             /* X */
		}
	}

	rect.left = 0;
	rect.top = 0;
	rect.right = (UINT16)width;
	rect.bottom = (UINT16)height;
	if (!region16_union_rect(&surface->invalidRegion, &surface->invalidRegion, &rect))
	{
		LeaveCriticalSection(&surface->lock);
		LeaveCriticalSection(&g_state.publish_lock);
		bridge_log("cannot mark surface region invalid");
		return false;
	}
	LeaveCriticalSection(&surface->lock);

	shadow_subsystem_frame_update(g_state.server->subsystem);

	EnterCriticalSection(&surface->lock);
	region16_clear(&surface->invalidRegion);
	LeaveCriticalSection(&surface->lock);

	LeaveCriticalSection(&g_state.publish_lock);
	g_state.frames_published++;
	return true;
}

static bool is_loopback(const char* address)
{
	if (!address)
		return false;
	return strcmp(address, "127.0.0.1") == 0 || strcmp(address, "::1") == 0 ||
	       strcmp(address, "[::1]") == 0 || strcmp(address, "localhost") == 0 ||
	       strncmp(address, "127.", 4) == 0;
}

static char* make_bind_spec(const char* address)
{
	size_t length = strlen(address) + 32;
	char* spec = (char*)malloc(length);
	if (!spec)
		return NULL;

	if (address[0] == '[')
		(void)snprintf(spec, length, "bind-address,%s", address);
	else if (strchr(address, ':'))
		(void)snprintf(spec, length, "bind-address,[%s]", address);
	else
		(void)snprintf(spec, length, "bind-address,%s", address);
	return spec;
}

static bool configure_server(rdpShadowServer* server, const BridgeOptions* options,
                             const OzfrFrame* first_frame)
{
	rdpSettings* settings = server->settings;
	char* bind_spec = make_bind_spec(options->bind_address);

	if (!bind_spec)
		return false;

	server->port = options->port;
	server->ipcSocket = bind_spec;
	server->mayView = TRUE;
	server->mayInteract = FALSE;
	server->authentication = TRUE;
	server->ShowMouseCursor = FALSE;
	/* Enforce the viewer limit in Authenticate so rejected clients receive a
	 * clear RDP insufficient-privileges error.  Keep a small accept bound so a
	 * pre-auth flood cannot grow without limit. */
	server->maxClientsConnected =
	    options->max_viewers > 0 ? (size_t)options->max_viewers + 4 : 0;
	g_state.max_viewers = options->max_viewers;

	if (options->config_dir)
	{
		server->ConfigPath = strdup(options->config_dir);
		if (!server->ConfigPath)
			return false;
	}

	if (!freerdp_settings_set_bool(settings, FreeRDP_TlsSecurity, TRUE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_NlaSecurity, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_ExtSecurity, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_RdpSecurity, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_UseRdpSecurityLayer, FALSE))
		return false;

	/* Keep the direct path deterministic: FreeRDP sends bitmap updates from
	 * the surface instead of negotiating GFX/RFX/NSC codecs.  This bridge does
	 * not implement RDPEGFX or H.264 itself. */
	if (!freerdp_settings_set_bool(settings, FreeRDP_SupportGraphicsPipeline, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_GfxH264, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_GfxAVC444, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_GfxAVC444v2, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_GfxProgressive, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_GfxPlanar, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_RemoteFxCodec, FALSE) ||
	    !freerdp_settings_set_bool(settings, FreeRDP_NSCodec, FALSE))
		return false;

	if (!freerdp_settings_set_uint32(settings, FreeRDP_DesktopWidth, first_frame->width) ||
	    !freerdp_settings_set_uint32(settings, FreeRDP_DesktopHeight, first_frame->height) ||
	    !freerdp_settings_set_uint32(settings, FreeRDP_ColorDepth, 32))
		return false;

	return true;
}

static void print_viewer_command(const BridgeOptions* options)
{
	const char* user = options->auth_file ? "<file-user>" : options->token_user;
	const char* secret = options->auth_file ? "<file-secret>" : "<token-above>";
	bridge_log("viewer command:");
	fprintf(stderr,
	        "  xfreerdp3 /v:%s:%u /u:%s /p:%s /sec:tls /cert:ignore /size:%ux%u "
	        "-gfx -rfx /bpp:32\n",
	        options->bind_address, options->port, user, secret, g_state.width, g_state.height);
	fflush(stderr);
}

int main(int argc, char** argv)
{
	BridgeOptions options;
	OzfrFrame first_frame;
	rdpShadowServer* server = NULL;
	int result = 1;

	memset(&first_frame, 0, sizeof(first_frame));
	first_frame.fd = -1;

	(void)signal(SIGPIPE, SIG_IGN);
	(void)signal(SIGINT, on_signal);
	(void)signal(SIGTERM, on_signal);

	if (!parse_options(argc, argv, &options))
	{
		usage(stderr, argv[0]);
		free_options(&options);
		return 2;
	}
	if (options.help)
	{
		usage(stdout, argv[0]);
		free_options(&options);
		return 0;
	}

	memset(&g_state, 0, sizeof(g_state));
	if (!InitializeCriticalSectionAndSpinCount(&g_state.lock, 4000) ||
	    !InitializeCriticalSectionAndSpinCount(&g_state.publish_lock, 4000))
	{
		bridge_log("cannot initialize locks");
		free_options(&options);
		return 1;
	}

	if (options.auth_file)
	{
		if (!load_auth_file(&g_state, options.auth_file))
			goto cleanup_locks;
	}
	else
	{
		char token[BRIDGE_TOKEN_BYTES * 2 + 1];
		if (!generate_token(&g_state, &options, token, sizeof(token)))
			goto cleanup_locks;
	}

	/* Read the first frame before FreeRDP builds its screen, so the primary
	 * surface has the stream's dimensions from the start. */
	if (read_frame(&first_frame, false) != 0)
		goto cleanup_credentials;
	g_state.width = first_frame.width;
	g_state.height = first_frame.height;

	shadow_subsystem_set_entry(bridge_subsystem_entry);

	server = shadow_server_new();
	if (!server)
	{
		bridge_log("shadow_server_new failed");
		goto cleanup_frame;
	}
	g_state.server = server;

	if (!configure_server(server, &options, &first_frame))
	{
		bridge_log("cannot configure FreeRDP shadow server");
		goto cleanup_frame;
	}

	if (shadow_server_init(server) < 0)
	{
		bridge_log("shadow_server_init failed (TLS certificate generation?)");
		goto cleanup_frame;
	}

	if (shadow_server_start(server) < 0)
	{
		bridge_log("shadow_server_start failed (port %u in use?)", options.port);
		goto cleanup_server;
	}

	if (!is_loopback(options.bind_address))
		bridge_log("warning: binding %s is not loopback; TLS and viewer credentials are required",
		           options.bind_address);

	bridge_log("direct surface %ux%u on rdp://%s:%u (TLS required, view-only, max %u viewers)",
	           g_state.width, g_state.height, options.bind_address, options.port,
	           options.max_viewers);
	print_viewer_command(&options);

	if (!publish_frame(&first_frame))
	{
		bridge_log("cannot publish initial frame");
		goto cleanup_started;
	}

	{
		OzfrFrame frame;
		memset(&frame, 0, sizeof(frame));
		frame.fd = -1;
		free_frame(&first_frame);

		while (!g_stop)
		{
			struct pollfd descriptor;
			int ready = 0;

			descriptor.fd = STDIN_FILENO;
			descriptor.events = POLLIN;
			descriptor.revents = 0;
			ready = poll(&descriptor, 1, 500);
			if (ready < 0)
			{
				if (errno == EINTR)
					continue;
				bridge_log("polling stdin: %s", strerror(errno));
				break;
			}
			if (ready == 0)
				continue;
			if (descriptor.revents & (POLLERR | POLLNVAL))
				break;
			if (descriptor.revents & (POLLHUP | POLLIN))
			{
				int status = read_frame(&frame, true);
				if (status != 0)
					break;
				if (frame.fd < 0)
					break; /* clean EOF between frames */
				if (frame.width != g_state.width || frame.height != g_state.height)
				{
					bridge_log("OZFR dimensions changed mid-stream; reconnect viewers");
					free_frame(&frame);
					break;
				}
				if (!publish_frame(&frame))
				{
					free_frame(&frame);
					break;
				}
				free_frame(&frame);
			}
		}
	}

	result = 0;

cleanup_started:
	shadow_server_stop(server);
cleanup_server:
	shadow_server_uninit(server);
cleanup_frame:
	if (server)
		shadow_server_free(server);
	free_frame(&first_frame);
cleanup_credentials:
	bridge_log("stopped after %llu frame(s); %llu viewer(s) allowed, %llu rejected",
	           (unsigned long long)g_state.frames_published,
	           (unsigned long long)g_state.viewers_allowed,
	           (unsigned long long)g_state.viewers_rejected);
	free_credentials(&g_state);
cleanup_locks:
	DeleteCriticalSection(&g_state.publish_lock);
	DeleteCriticalSection(&g_state.lock);
	free_options(&options);
	return result;
}
