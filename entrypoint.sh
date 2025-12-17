#!/bin/sh
 
# Set ownership of the volume directory to the service user
chown -R appuser:appuser /app/data
 
# Execute the main command (passed as arguments to the script)
exec "$@"
