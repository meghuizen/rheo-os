// A stock dynamically-linked (PIE) C++ hello - the four-library production
// shape (docs/LINUX-COMPAT.md L7, GOAL-DYN-MULTILIB). Using std::string and
// std::vector pulls in libstdc++.so.6, which drags libgcc_s.so.1 (unwind) and
// libm.so.6, so ld.so must load four shared objects and run C++ runtime init
// (static constructors, exception-unwind tables) - well beyond dmath's two.
#include <string>
#include <vector>
#include <cstdio>

int main() {
    std::vector<std::string> v{"hello", "from", "dynamic", "C++"};
    std::string s;
    for (auto& w : v) { s += w; s += ' '; }
    std::printf("dcpp: %s(%zu)\n", s.c_str(), s.size());
    return (int)(s.size() % 100); // exit 23
}
