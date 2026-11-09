# Runs under GNU make from the store, with BusyBox as the shell.
all:
	@{ \
	  echo "make: $(MAKE_VERSION)"; \
	  echo "shell: $$(readlink /bin/sh)"; \
	  echo "banner: $$(busybox | head -n 1)"; \
	  echo "host libc present: $$([ -e /usr/lib/libc.so.6 ] && echo yes || echo no)"; \
	  echo "host gcc present: $$([ -e /usr/bin/gcc ] && echo yes || echo no)"; \
	  echo "root entries: $$(ls / | tr '\n' ' ')"; \
	} > result.txt
