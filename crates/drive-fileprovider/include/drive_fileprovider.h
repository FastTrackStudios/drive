// The C ABI of `drive-fileprovider`: what Drive's macOS File Provider
// extension links (docs/fileprovider.md).
//
// Error protocol: a function returning a string returns NULL on failure,
// one returning int returns -1; the reason is in dfp_last_error() until
// the next call on the same thread. Every string returned is the
// caller's, freed with dfp_free().

#ifndef DRIVE_FILEPROVIDER_H
#define DRIVE_FILEPROVIDER_H

#ifdef __cplusplus
extern "C" {
#endif

/// Where the agent is: the path of an `app.sock` it serves (for the
/// extension, the one in its app group's container). Connects now; later
/// calls reconnect by themselves if the agent restarts. 0, or -1.
int dfp_connect(const char *socket_path);

/// One request to the agent, as JSON (`{"op":"fp_list","parent":"root"}`;
/// the ops are in docs/fileprovider.md); its answer, as JSON. NULL on
/// failure: the agent's reason (or why it could not be asked) is in
/// dfp_last_error().
char *dfp_call(const char *request_json);

/// Why the last call on this thread failed, or NULL.
char *dfp_last_error(void);

/// Free a string this library returned.
void dfp_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* DRIVE_FILEPROVIDER_H */
