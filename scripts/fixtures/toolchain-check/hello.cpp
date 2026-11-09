// Uses the parts of C++ that need libstdc++ and the unwinder: containers,
// strings, streams, and an exception thrown and caught.
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

int main() {
    std::vector<std::string> words{"hello", "from", "C++"};
    std::string out;
    for (const auto &w : words) {
        if (!out.empty()) out += ' ';
        out += w;
    }
    try {
        throw std::runtime_error("caught");
    } catch (const std::exception &e) {
        out += std::string(" (") + e.what() + ")";
    }
    std::cout << out << '\n';
    return 0;
}
