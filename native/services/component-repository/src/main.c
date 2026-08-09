#include <component_repository.h>

int main(void) {
    for (;;) {
        int result = component_repository_serve_once();
        if (result <= 0) {
            return result == 0 ? 0 : 1;
        }
    }
}
