# Runs under GNU make from the store, with BusyBox as the shell and the
# store's gcc as the compiler. $(RECIPE_DIR) is where forge mounts this recipe.
CC  = gcc
CXX = g++

all: result.txt

hello-c: $(RECIPE_DIR)/hello.c
	$(CC) -O2 -Wall -o $@ $<

hello-c-static: $(RECIPE_DIR)/hello.c
	$(CC) -O2 -static -o $@ $<

hello-cxx: $(RECIPE_DIR)/hello.cpp
	$(CXX) -O2 -Wall -std=c++17 -o $@ $<

hello-cxx-static: $(RECIPE_DIR)/hello.cpp
	$(CXX) -O2 -std=c++17 -static -o $@ $<

result.txt: hello-c hello-c-static hello-cxx hello-cxx-static
	@{ \
	  echo "compiler: $$($(CC) --version | head -n 1)"; \
	  echo "target: $$($(CC) -dumpmachine)"; \
	  echo "c dynamic: $$(./hello-c)"; \
	  echo "c static: $$(./hello-c-static)"; \
	  echo "c++ dynamic: $$(./hello-cxx)"; \
	  echo "c++ static: $$(./hello-cxx-static)"; \
	  echo "loader: $$(readelf -lW hello-c | grep 'program interpreter')"; \
	  echo "static has no interpreter: $$([ -z "$$(readelf -lW hello-c-static | grep 'program interpreter')" ] && echo yes || echo no)"; \
	  echo "host libc present: $$([ -e /usr/lib/libc.so.6 ] && echo yes || echo no)"; \
	  echo "host gcc present: $$([ -e /usr/lib/gcc/x86_64-pc-linux-gnu ] && echo yes || echo no)"; \
	} > $@
