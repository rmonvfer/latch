#import "../src/updater.m"
#include <assert.h>
#include <sys/wait.h>

static NSBundle *fixtureBundle(NSString *directory, NSString *name) {
    NSString *path = [directory stringByAppendingPathComponent:[name stringByAppendingString:@".app"]];
    NSString *contents = [path stringByAppendingPathComponent:@"Contents"];
    assert([[NSFileManager defaultManager] createDirectoryAtPath:contents withIntermediateDirectories:YES attributes:nil error:nil]);
    NSDictionary *info = @{
        @"CFBundleIdentifier": @"me.egrati.latch.relaunch-test",
        @"CFBundleVersion": @"100001",
    };
    assert([info writeToFile:[contents stringByAppendingPathComponent:@"Info.plist"] atomically:YES]);
    NSBundle *bundle = [NSBundle bundleWithPath:path];
    assert(bundle != nil);
    return bundle;
}

static NSDictionary *record(NSString *home, NSString *build, NSTimeInterval timestamp, pid_t pid) {
    return @{@"configHome": home, @"build": build, @"timestamp": @(timestamp), @"pid": @(pid)};
}

static void seed(NSUserDefaults *defaults, NSBundle *bundle, id value) {
    [defaults setObject:@{bundleContextPath(bundle): value} forKey:relaunchContextsKey];
    assert([defaults synchronize]);
}

static void assertConsumed(NSUserDefaults *defaults, NSBundle *bundle) {
    assert(relaunchContexts(defaults)[bundleContextPath(bundle)] == nil);
}

int main(void) {
    @autoreleasepool {
        NSString *identifier = [@"me.egrati.latch.relaunch-test." stringByAppendingString:NSUUID.UUID.UUIDString];
        NSUserDefaults *defaults = [[NSUserDefaults alloc] initWithSuiteName:identifier];
        NSString *directory = [NSTemporaryDirectory() stringByAppendingPathComponent:identifier];
        NSBundle *bundle = fixtureBundle(directory, @"Installed");
        NSBundle *other = fixtureBundle(directory, @"Other");
        pid_t dead = fork();
        assert(dead >= 0);
        if (dead == 0) _exit(0);
        assert(waitpid(dead, NULL, 0) == dead);
        assert(kill(dead, 0) == -1 && errno == ESRCH);
        NSTimeInterval now = [NSDate timeIntervalSinceReferenceDate];
        NSDictionary *valid = record(@"/tmp/latch-relaunch-profile", @"100000", now, dead);
        unsetenv("XDG_CONFIG_HOME");

        seed(defaults, bundle, valid);
        assert(restoreRelaunchContext(bundle, defaults));
        assert(strcmp(getenv("XDG_CONFIG_HOME"), "/tmp/latch-relaunch-profile") == 0);
        assertConsumed(defaults, bundle);
        unsetenv("XDG_CONFIG_HOME");
        assert(!restoreRelaunchContext(bundle, defaults));

        seed(defaults, bundle, valid);
        setenv("XDG_CONFIG_HOME", "/tmp/explicit-profile", 1);
        assert(!restoreRelaunchContext(bundle, defaults));
        assert(strcmp(getenv("XDG_CONFIG_HOME"), "/tmp/explicit-profile") == 0);
        assertConsumed(defaults, bundle);
        unsetenv("XDG_CONFIG_HOME");

        NSArray *invalid = @[
            record(@"/tmp/profile", @"100001", now, dead),
            record(@"/tmp/profile", @"100000", now - 601, dead),
            record(@"/tmp/profile", @"100000", now + 601, dead),
            record(@"/tmp/profile", @"100000", now, getpid()),
            record(@"relative/profile", @"100000", now, dead),
            @[@"invalid record"],
            @{@"configHome": @12, @"build": @"100000", @"timestamp": @(now), @"pid": @(dead)},
        ];
        for (id value in invalid) {
            seed(defaults, bundle, value);
            assert(!restoreRelaunchContext(bundle, defaults));
            assert(getenv("XDG_CONFIG_HOME") == NULL);
            assertConsumed(defaults, bundle);
        }

        [defaults setObject:@{
            bundleContextPath(bundle): valid,
            bundleContextPath(other): record(@"/tmp/other-profile", @"100000", now, dead),
        } forKey:relaunchContextsKey];
        assert(restoreRelaunchContext(bundle, defaults));
        assert(relaunchContexts(defaults)[bundleContextPath(other)] != nil);
        unsetenv("XDG_CONFIG_HOME");
        assert(restoreRelaunchContext(other, defaults));
        assert(strcmp(getenv("XDG_CONFIG_HOME"), "/tmp/other-profile") == 0);
        unsetenv("XDG_CONFIG_HOME");

        setenv("XDG_CONFIG_HOME", "nested/../relative-profile", 1);
        saveRelaunchContext(bundle, defaults);
        NSString *expected = [[NSFileManager defaultManager].currentDirectoryPath stringByAppendingPathComponent:@"relative-profile"];
        assert([relaunchContexts(defaults)[bundleContextPath(bundle)][@"configHome"] isEqualToString:expected]);
        unsetenv("XDG_CONFIG_HOME");
        saveRelaunchContext(bundle, defaults);
        assertConsumed(defaults, bundle);

        [defaults removePersistentDomainForName:identifier];
        [defaults synchronize];
        assert([[NSFileManager defaultManager] removeItemAtPath:directory error:nil]);
        puts("Sparkle relaunch context regression checks passed");
    }
    return 0;
}
