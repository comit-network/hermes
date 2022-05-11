import { ExternalLinkIcon } from "@chakra-ui/icons";
import { Box, Button, Center, Divider, HStack, Link, Text, useColorModeValue } from "@chakra-ui/react";
import { FeedbackFish } from "@feedback-fish/react";
import * as React from "react";
import { FaRegCommentDots } from "react-icons/all";
import { FAQ_URL, FOOTER_HEIGHT } from "../App";
import { SocialLinks } from "./SocialLinks";

function TextDivider() {
    return <Divider orientation={"vertical"} borderColor={useColorModeValue("black", "white")} height={"20px"} />;
}

interface FooterProps {
    taker_id: string;
}

export default function Footer({ taker_id }: FooterProps) {
    return (
        <Box
            bg={useColorModeValue("gray.100", "gray.900")}
            color={useColorModeValue("gray.700", "gray.200")}
        >
            <Center>
                <HStack h={`${FOOTER_HEIGHT}px`} alignItems={"center"}>
                    <Link
                        href={FAQ_URL}
                        isExternal
                    >
                        <HStack>
                            <Text fontSize={"20"} fontWeight={"bold"}>FAQ</Text>
                            <ExternalLinkIcon boxSize={5} />
                        </HStack>
                    </Link>
                    <TextDivider />
                    <Text fontSize={"20"} fontWeight={"bold"} display={["none", "none", "inherit"]}>Contact us:</Text>
                    <SocialLinks />
                    <TextDivider />
                    <FeedbackFish
                        projectId="c1260a96cdb3d8"
                        metadata={{ position: "footer", customerId: taker_id }}
                    >
                        <Button
                            fontSize={"20"}
                            color={useColorModeValue("black", "white")}
                            leftIcon={<FaRegCommentDots />}
                            variant={"ghost"}
                        >
                            <Text display={["none", "none", "inherit"]}>Send Feedback</Text>
                        </Button>
                    </FeedbackFish>
                </HStack>
            </Center>
        </Box>
    );
}
