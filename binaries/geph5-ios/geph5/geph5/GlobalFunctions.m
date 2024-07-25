//
//  GlobalFunctions.m
//  geph5
//
//  Created by Lucas Privat on 25.07.24.
//

#import <Foundation/Foundation.h>
#import <geph5-Swift.h>
#import "GlobalFunctions.h"

@implementation GlobalFunctionsObjc


// Unfortunnately calling the swift code annotated with @objc directly from rust is not possible,
// I've created a issue here to ask about this: https://github.com/madsmtm/objc2/issues/642
+ (void)objcDummyFnWithData:(NSString*) data {
    [GlobalFunctions objcDummyFnWithData:data];
}

@end

