// Runs inside the daemon, so macOS evaluates the daemon's responsible app.
// A ready UDP path proves routing permission only, never vehicle delivery.
#import <Network/Network.h>
#import <dispatch/dispatch.h>
#import <stdatomic.h>

int uxduck_local_network_path(const char *host, const char *port) {
    if (@available(macOS 11.0, *)) {
        nw_endpoint_t endpoint = nw_endpoint_create_host(host, port);
        nw_parameters_t parameters = nw_parameters_create_secure_udp(
            NW_PARAMETERS_DISABLE_PROTOCOL, NW_PARAMETERS_DEFAULT_CONFIGURATION);
        nw_connection_t connection = nw_connection_create(endpoint, parameters);
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        __block atomic_int result;
        atomic_init(&result, -1);
        nw_connection_set_queue(connection, dispatch_get_global_queue(QOS_CLASS_UTILITY, 0));
        nw_connection_set_state_changed_handler(connection, ^(nw_connection_state_t state, nw_error_t error) {
            (void)error;
            nw_path_t path = nw_connection_copy_current_path(connection);
            if (path && nw_path_get_unsatisfied_reason(path) == nw_path_unsatisfied_reason_local_network_denied) {
                atomic_store(&result, 1);
                dispatch_semaphore_signal(done);
            } else if (state == nw_connection_state_ready) {
                atomic_store(&result, 0);
                dispatch_semaphore_signal(done);
            } else if (state == nw_connection_state_failed) {
                dispatch_semaphore_signal(done);
            }
        });
        nw_connection_start(connection);
        long timed_out = dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 750 * NSEC_PER_MSEC));
        nw_connection_cancel(connection);
        nw_connection_set_state_changed_handler(connection, NULL);
        return timed_out ? -1 : atomic_load(&result);
    }
    return -1;
}
