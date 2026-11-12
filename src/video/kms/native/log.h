// Error logging for the KMS capture: one line on stderr, which is the
// service's log, prefixed with the file's LOG_MODULE.

#ifndef KMS_LOG_H
#define KMS_LOG_H

#include <cstdio>
#include <string>

#define LOG_ERROR(message)                                                     \
  std::fprintf(stderr, "[%s] %s\n", LOG_MODULE, std::string(message).c_str())

#endif
