// macOS: this Mac's sleep and wake (IOKit's system power notices), whether
// it's in a dark wake now, and waking it for real. See sleep.rs.

#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/IOMessage.h>
#include <IOKit/pwr_mgt/IOPM.h>
#include <IOKit/pwr_mgt/IOPMLib.h>
#include <pthread.h>

typedef void (*muxen_power_handler)(int);

static io_connect_t root_port;
static muxen_power_handler on_change;

// macOS tells these to apps when the Mac leaves and reaches a full wake: a
// dark wake (a moment awake for the network or upkeep, its display off)
// isn't said to them, so between the two the Mac counts as asleep.
static void power_changed(void *refcon, io_service_t service, natural_t type, void *argument) {
    (void)refcon;
    (void)service;
    switch (type) {
    case kIOMessageCanSystemSleep:
        // An idle sleep asks first. Never the one to say no.
        IOAllowPowerChange(root_port, (long)argument);
        break;
    case kIOMessageSystemWillSleep:
        // Returns once the linked apps were told (bounded: see sleep.rs);
        // macOS waits for the answer, 30 seconds at most.
        on_change(1);
        IOAllowPowerChange(root_port, (long)argument);
        break;
    case kIOMessageSystemHasPoweredOn:
        on_change(2);
        break;
    default:
        break;
    }
}

static void *watch(void *unused) {
    (void)unused;
    IONotificationPortRef port = NULL;
    io_object_t notifier = 0;
    root_port = IORegisterForSystemPower(NULL, &port, power_changed, &notifier);
    if (root_port == MACH_PORT_NULL || port == NULL) {
        return NULL;
    }
    CFRunLoopAddSource(CFRunLoopGetCurrent(), IONotificationPortGetRunLoopSource(port), kCFRunLoopCommonModes);
    CFRunLoopRun();
    return NULL;
}

// Calls `handler` with 1 when the Mac is about to sleep (it sleeps once the
// handler returns) and with 2 when it has woken, from a thread of its own.
// 0 when that thread couldn't be started.
int muxen_power_watch(muxen_power_handler handler) {
    on_change = handler;
    pthread_t thread;
    if (pthread_create(&thread, NULL, watch, NULL) != 0) {
        return 0;
    }
    pthread_detach(thread);
    return 1;
}

static io_registry_entry_t root_domain(void) {
    return IORegistryEntryFromPath(MACH_PORT_NULL, kIOPowerPlane ":/IOPowerConnection/IOPMrootDomain");
}

// What's powered now, as kIOPMSystemCapability bits (CPU 1, graphics 2,
// audio 4, network 8): a full wake has graphics, a dark wake doesn't. -1 when
// macOS doesn't say.
int muxen_power_capabilities(void) {
    io_registry_entry_t root = root_domain();
    if (root == MACH_PORT_NULL) {
        return -1;
    }
    int capabilities = -1;
    CFTypeRef value = IORegistryEntryCreateCFProperty(root, CFSTR("System Capabilities"), kCFAllocatorDefault, 0);
    if (value != NULL) {
        if (CFGetTypeID(value) == CFNumberGetTypeID()) {
            CFNumberGetValue((CFNumberRef)value, kCFNumberIntType, &capabilities);
        }
        CFRelease(value);
    }
    IOObjectRelease(root);
    return capabilities;
}

// A laptop's lid: 1 closed, 0 open, -1 for a Mac without one.
int muxen_power_lid_closed(void) {
    io_registry_entry_t root = root_domain();
    if (root == MACH_PORT_NULL) {
        return -1;
    }
    int closed = -1;
    CFTypeRef value = IORegistryEntryCreateCFProperty(root, CFSTR(kAppleClamshellStateKey), kCFAllocatorDefault, 0);
    if (value != NULL) {
        if (CFGetTypeID(value) == CFBooleanGetTypeID()) {
            closed = CFBooleanGetValue((CFBooleanRef)value) ? 1 : 0;
        }
        CFRelease(value);
    }
    IOObjectRelease(root);
    return closed;
}

// Says someone is using this Mac: one in a dark wake wakes fully and turns
// its display on, as a key press would. 1 when macOS took it.
int muxen_power_wake(void) {
    IOPMAssertionID assertion = 0;
    return IOPMAssertionDeclareUserActivity(CFSTR("Woken from Muxin"), kIOPMUserActiveLocal, &assertion) == kIOReturnSuccess ? 1 : 0;
}
