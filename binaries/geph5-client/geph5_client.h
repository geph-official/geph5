/**
 * @file geph5_client.h
 * @brief C interface for the Geph5 client library
 */

#ifndef GEPH5_CLIENT_H
#define GEPH5_CLIENT_H

#include <stdint.h>

/**
 * @brief Starts the Geph5 client with the provided configuration
 * 
 * @param cfg JSON string containing client configuration
 * @return int 0 on success, non-zero on failure
 */
int start_client(const char* cfg);

/**
 * @brief Sends an RPC request to the daemon
 * 
 * @param jrpc_req JSON-RPC request string
 * @param out_buf Buffer to store the response
 * @param out_buflen Length of the output buffer
 * @return int Length of response on success, negative value on error:
 *             -1 if daemon not started
 *             -2 for JSON-RPC error
 *             -3 if buffer not big enough
 *             -4 if writing to buffer failed
 */
int daemon_rpc(const char* jrpc_req, char* out_buf, int out_buflen);

/**
 * @brief Sends a VPN packet
 * 
 * @param pkt Packet data to send
 * @param pkt_len Length of the packet
 * @return int 0 on success, -1 on failure
 */
int send_pkt(const char* pkt, int pkt_len);

/**
 * @brief Receives a VPN packet
 * 
 * @param out_buf Buffer to store the received packet
 * @param out_buflen Length of the output buffer
 * @return int Length of packet on success, negative value on error
 */
int recv_pkt(char* out_buf, int out_buflen);

#endif /* GEPH5_CLIENT_H */